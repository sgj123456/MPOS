//! The display: the task that owns the GPU and paints terminal screens to it.
//!
//! The pure pixel math lives with the grid it paints, in
//! [`crate::vfs::term::blit`], so the host can test it. What is here is the
//! kernel-only half: the single task that owns the device and is the only
//! caller of `flush`.

#[cfg(target_os = "none")]
pub mod render;
