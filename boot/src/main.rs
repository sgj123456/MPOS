//! The firmware entry point: a UEFI application whose whole body is the
//! loader's library.
//!
//! The split is deliberate. The *sequence* - what to read, what to map, when to
//! leave boot services - is `mpos_boot::run`, and it is a library function
//! precisely so that the kernel, which `NOTES.md` §14.7 schedules to become the
//! UEFI application itself, can call it in-process instead of loading an image
//! that does. Until that lands this binary is the firmware's entry point, and
//! all it does is hand `run` the `Status` the firmware wants and give `uefi` the
//! `efi_main` symbol the PE needs.

#![no_std]
#![no_main]

use uefi::prelude::*;

#[entry]
fn main() -> Status {
    mpos_boot::run()
}
