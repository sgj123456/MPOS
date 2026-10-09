//! The machine layer as the *loader* sees it: the page tables a bootloader
//! builds before it jumps, the wire its progress lines go out on, and the last
//! instruction that hands the machine over.
//!
//! # What this is, and why it is not `crate::arch`
//!
//! [`crate::arch`] is the machine layer of a *running kernel*: it is compiled
//! for `target_os = "none"` only, it is written against `mpos-bootinfo`, and
//! every page it maps is reached through a direct map that already exists. This
//! module is the same machine one step earlier - under the firmware's identity
//! map, addressed by physical address, with no `BootInfo`, no frame allocator
//! and no `hal` instance, because there is nothing yet to hold one.
//!
//! The two write the *same descriptor format*, and that is exactly the trap:
//! this file and `crate::arch::<machine>::paging` are near neighbours that must
//! not be merged. `paging` decodes as well as encodes - the kernel translates
//! through what it wrote and reads the permissions back - and runs on tables
//! the kernel owns. This encodes only, a 4 KiB leaf at a time, and runs on
//! tables built out of an allocation the firmware handed the loader. A caller
//! that reached this after the jump would be writing into an identity map that
//! no longer exists; a caller that reached `paging` before the jump would be
//! reading a `BootInfo` nobody has built. Keeping them in separate modules,
//! with this one a separate *tree*, is what makes the difference a name rather
//! than a paragraph someone has to remember.
//!
//! # Why it compiles where the rest of the crate does not
//!
//! `mpos-boot` is a UEFI application - a PE32+ program built for
//! `{x86_64,aarch64}-unknown-uefi` - while the kernel is a bare-metal ELF for
//! `*-unknown-none`. So `crate::hal` and `crate::arch` are gated on
//! `target_os = "none"` and this module is gated on `target_os = "uefi"`: a
//! loader build compiles the machine code it needs and none of the kernel's.
//! When the kernel becomes a UEFI stub of its own (`NOTES.md` §14.6, item 5) the
//! kernel gate widens to `any(…)` and this sentence changes with it - which is
//! the one place in this crate where that is true.
//!
//! # Who decides what
//!
//! The loader decides where frames come from: it allocates one contiguous run
//! of `LOADER_DATA` sized from RAM before the first frame is taken, and hands it
//! over. This module decides what gets written into those frames. [`Frames`] is
//! that seam - `PageTables` cannot allocate anything the loader did not give it,
//! and the trait is monomorphised onto the loader's own `Pool`, so the
//! arrangement costs nothing at run time and needs no `dyn`.
//!
//! Progress lines go the other way. The loader owns the format (`mpos-boot: `
//! and a trailing newline) and this module has only the wire, so the loader
//! registers a sink with [`set_log`]. That is the same shape as
//! `hal::kernel`'s registration and for the same reason: the layer that has the
//! facts is not the layer that owns the presentation. Register it before
//! anything that logs - a line emitted with no sink registered is dropped, not
//! panicked, because the only witness to a boot that panics while trying to
//! print would be the line that could not be printed.

use mpos_vm::PAGE_SIZE as PAGE_BYTES;

/// The granule every descriptor here is written in, and the unit
/// `PageTables::map` insists on.
pub(crate) const PAGE_SIZE: u64 = PAGE_BYTES as u64;

/// Where the loader's frames come from.
///
/// One method, because it is one question: a zeroed 4 KiB frame for a page
/// table. Zeroed is not an implementation detail of the loader's allocator - an
/// empty page-table entry is an absent one, so a frame straight from the
/// firmware still holding the last user's bytes would read as mappings wherever
/// bit 0 happened to be set.
pub trait Frames {
    /// A zeroed 4 KiB frame of physical memory, or a panic when none is left.
    ///
    /// A panic rather than an `Option`: the loader sizes its pool from the size
    /// of RAM before the first frame is taken (`mpos_boot::vm::pool_bytes_for`),
    /// so running out means the arithmetic and the machine disagree, and there
    /// is nothing useful to unwind to during boot.
    fn zeroed_frame(&mut self) -> u64;
}

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::{
    FIRMWARE_PUBLISHES_DEVICE_TREE, PageTables, enter, extra_mapped_bytes, map_extra, serial_init,
    serial_write,
};

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::{
    FIRMWARE_PUBLISHES_DEVICE_TREE, PageTables, enter, extra_mapped_bytes, map_extra, serial_init,
    serial_write,
};

/// The loader's progress sink: one line, already formatted.
///
/// A `static mut` written once and read on every progress line, which is the
/// same bargain `hal::kernel::REGISTERED` makes: one writer, it happens before
/// anything that can log, and the value never changes afterwards.
static mut LOG: Option<fn(&str)> = None;

/// Point this module's progress lines at the loader's formatter.
///
/// Call it after [`serial_init`] and before the first line that matters. A line
/// logged before this arrives is dropped silently; a line logged after it is the
/// loader's own text, prefix and all.
pub fn set_log(sink: fn(&str)) {
    // SAFETY: boot is single-threaded and this runs before the first progress
    // line, so no reader can be looking at the slot.
    unsafe { *core::ptr::addr_of_mut!(LOG) = Some(sink) };
}

/// One progress line, through whatever [`set_log`] registered.
///
/// Dropped when nothing is registered. During boot there is no console to fall
/// back on, no allocator to format a complaint with, and nothing above to catch
/// a panic - so the honest behaviour is to lose the line rather than the
/// machine.
///
/// Public and not `pub(crate)`, because it is this module's half of the pair
/// `set_log` opens: a machine that has a line to report calls this, and the
/// loader is free to call it for a line that belongs to the seam rather than to
/// either side. One machine's arm happens to have nothing to say and never calls
/// it, which is a fact about that machine and not a hole in the interface.
pub fn log(message: &str) {
    // SAFETY: `LOG` is a `Copy` value in a slot that is written once before any
    // reader exists; reading it through a raw pointer avoids taking a reference
    // to a `static mut` (edition 2024's `static_mut_refs`).
    let sink = unsafe { *core::ptr::addr_of!(LOG) };
    if let Some(sink) = sink {
        sink(message);
    }
}
