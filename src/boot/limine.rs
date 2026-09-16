//! Limine boot protocol requests, and their translation into `boot::BootInfo`.
//!
//! The *order* of the statics below is load-bearing. Limine discovers requests
//! by scanning the loaded image linearly: start marker, then the base revision
//! tag, then every `COMMON_MAGIC`-prefixed request, stopping at the end marker.
//! `arch/x86_64/linker.ld` places these sections in `.data` in that order.

use limine::memmap as limine_memmap;
use limine::request::{
    BootloaderInfoRequest, ExecutableAddressRequest, ExecutableCmdlineRequest, HhdmRequest,
    MemmapRequest, ModulesRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

use super::{BootInfo, MAX_MEM_REGIONS, MemKind, MemRegion};

/// Highest base revision our pinned Limine implements. Limine v9.6.7 defines
/// `SUPPORTED_BASE_REVISION 3`; the `limine` crate advertises 6. Asking for more
/// than the bootloader implements gets clamped and leaves `is_supported()` false,
/// so this must match the bootloader we ship, not the crate's maximum.
pub const BASE_REVISION: u64 = 3;

#[unsafe(link_section = ".limine_requests_start_marker")]
#[used]
static START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[unsafe(link_section = ".limine_requests")]
#[used]
static BASE_REV: BaseRevision = BaseRevision::with_revision(BASE_REVISION);

#[unsafe(link_section = ".limine_requests")]
#[used]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[unsafe(link_section = ".limine_requests")]
#[used]
static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

#[unsafe(link_section = ".limine_requests")]
#[used]
static BOOTLOADER_INFO_REQUEST: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[unsafe(link_section = ".limine_requests")]
#[used]
static EXECUTABLE_ADDRESS_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[unsafe(link_section = ".limine_requests")]
#[used]
static MODULES_REQUEST: ModulesRequest = ModulesRequest::new();

/// The command line the bootloader was given for *this executable* - the
/// `cmdline:` line of limine.conf. It is how a machine is told what to boot
/// into, and the initramfs's own init reads it from /proc/cmdline.
#[unsafe(link_section = ".limine_requests")]
#[used]
static CMDLINE_REQUEST: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

#[unsafe(link_section = ".limine_requests_end_marker")]
#[used]
static END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

/// The kernel command line, as the bootloader was given it. Empty when the
/// config names none.
fn cmdline() -> &'static str {
    CMDLINE_REQUEST
        .response()
        .map(|response| response.cmdline())
        .unwrap_or("")
}

/// The module whose path names the initramfs, if the config loaded one.
fn initramfs_module() -> Option<&'static [u8]> {
    let modules = MODULES_REQUEST.response()?.modules();
    modules
        .iter()
        .find(|file| file.path().ends_with("initramfs.tar"))
        .map(|file| file.data())
}

fn translate_kind(raw: u64) -> MemKind {
    match raw {
        limine_memmap::MEMMAP_USABLE => MemKind::Usable,
        limine_memmap::MEMMAP_RESERVED | limine_memmap::MEMMAP_MAPPED_RESERVED => MemKind::Reserved,
        limine_memmap::MEMMAP_ACPI_RECLAIMABLE => MemKind::AcpiReclaimable,
        limine_memmap::MEMMAP_ACPI_NVS => MemKind::AcpiNvs,
        limine_memmap::MEMMAP_BAD_MEMORY => MemKind::BadMemory,
        limine_memmap::MEMMAP_BOOTLOADER_RECLAIMABLE => MemKind::BootloaderReclaimable,
        limine_memmap::MEMMAP_EXECUTABLE_AND_MODULES => MemKind::KernelAndModules,
        limine_memmap::MEMMAP_FRAMEBUFFER => MemKind::Framebuffer,
        _ => MemKind::Reserved,
    }
}

/// Read every Limine response into a `BootInfo`.
///
/// Runs before the heap exists, so it must not allocate.
pub fn collect() -> BootInfo {
    assert!(
        BASE_REV.is_supported(),
        "limine rejected base revision {}; refusing to continue",
        BASE_REVISION
    );

    let hhdm = HHDM_REQUEST
        .response()
        .expect("limine: HHDM request was not answered");
    let memmap = MEMMAP_REQUEST
        .response()
        .expect("limine: memmap request was not answered");
    let booter = BOOTLOADER_INFO_REQUEST
        .response()
        .expect("limine: bootloader info request was not answered");
    let exec_addr = EXECUTABLE_ADDRESS_REQUEST
        .response()
        .expect("limine: executable address request was not answered");

    let mut info = BootInfo {
        hhdm_offset: hhdm.offset as usize,
        kernel_phys_base: exec_addr.physical_base as usize,
        kernel_virt_base: exec_addr.virtual_base as usize,
        bootloader_name: booter.name(),
        bootloader_version: booter.version(),
        initramfs: initramfs_module(),
        regions: [MemRegion {
            base: 0,
            length: 0,
            kind: MemKind::Reserved,
        }; MAX_MEM_REGIONS],
        region_count: 0,
        cmdline: cmdline(),
    };

    for entry in memmap.entries() {
        if info.region_count == MAX_MEM_REGIONS {
            break;
        }
        info.regions[info.region_count] = MemRegion {
            base: entry.base as usize,
            length: entry.length as usize,
            kind: translate_kind(entry.type_),
        };
        info.region_count += 1;
    }

    info
}
