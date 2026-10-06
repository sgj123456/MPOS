//! What the kernel maps, independent of how any architecture maps it.
//!
//! The page-table *format* belongs to `arch`. The *plan* - which virtual ranges
//! point at which physical ranges, with which permissions - is the same idea on
//! every architecture, so it lives here and is unit-tested on the host. A port
//! consumes a list of [`Mapping`]s and does not get to invent policy.
//!
//! The vocabulary itself is the `mpos-vm` leaf now, re-exported under the names
//! it has always had so every `crate::mm::vm::` caller is unchanged. It moved
//! out because `hal::Paging` is written in it and the machine layer is becoming
//! its own crate, which cannot depend on the kernel (`PLAN.md` §14 S2b) - and
//! that is also why the tests that pin the plan went with it rather than
//! staying here to test a re-export.

pub use mpos_vm::{
    KernelSections, MapFlags, Mapping, PAGE_SHIFT, PAGE_SIZE, PageTaken, align_down, align_up,
    kernel_mappings,
};
