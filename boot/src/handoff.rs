//! The record this loader leaves for the kernel.
//!
//! Duplicated, field for field, from `src/boot/handoff.rs` in the kernel crate.
//! The two crates cannot share a definition - one is built for
//! `x86_64-unknown-uefi` and the other for `x86_64-unknown-none`, and neither
//! can depend on the other - so the pair is kept in step by [`MAGIC`], which is
//! written here and checked there. Changing a field means changing both files
//! and, if the change is not backwards compatible, the magic with them.
//!
//! The kinds below are the whole of the kernel's `MemKind`, not just the ones a
//! UEFI memory map produces: a mirror that carried only what this loader
//! happens to emit would stop being a mirror of anything.
#![allow(dead_code)]

/// `"MPOSBOOT"` as little-endian bytes.
pub const MAGIC: u64 = u64::from_le_bytes(*b"MPOSBOOT");

/// Must match `MAX_MEM_REGIONS` in the kernel's `boot` module.
pub const MAX_MEM_REGIONS: usize = 128;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Region {
    pub base: u64,
    pub length: u64,
    pub kind: u32,
    pub reserved: u32,
}

/// Region kinds, as the kernel's `MemKind` order.
pub const KIND_USABLE: u32 = 0;
pub const KIND_RESERVED: u32 = 1;
pub const KIND_ACPI_RECLAIMABLE: u32 = 2;
pub const KIND_ACPI_NVS: u32 = 3;
pub const KIND_BAD_MEMORY: u32 = 4;
pub const KIND_BOOTLOADER_RECLAIMABLE: u32 = 5;
pub const KIND_KERNEL_AND_MODULES: u32 = 6;
pub const KIND_FRAMEBUFFER: u32 = 7;

#[repr(C)]
pub struct Handoff {
    pub magic: u64,
    pub hhdm_offset: u64,
    pub kernel_phys_base: u64,
    pub kernel_virt_base: u64,
    pub initramfs_phys: u64,
    pub initramfs_len: u64,
    pub cmdline_phys: u64,
    pub cmdline_len: u64,
    /// Direct-map address of the bottom of the boot stack, where this loader
    /// left its canary. Zero when it left none.
    pub boot_stack_base: u64,
    pub region_count: u64,
    pub regions: [Region; MAX_MEM_REGIONS],
}
