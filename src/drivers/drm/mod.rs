//! DRM/KMS: a `/dev/dri/card0` a compositor can drive.
//!
//! weston's modern backend - and any DRM/KMS compositor - opens
//! `/dev/dri/card0`, enumerates a single CRTC/encoder/connector/plane, makes
//! CPU-writable *dumb buffers* (GEM), attaches them with `ADDFB`, and shows
//! them with `SETCRTC`/`PAGE_FLIP`, waiting on vblank events read back off
//! the card fd. There is no 3D here; GBM and its GL clients reach these same
//! dumb buffers from the other side, as PRIME dma-bufs (`drm.rs` exports a
//! handle to a descriptor, Mesa maps the descriptor, renders into it with
//! llvmpipe, and the compositor imports the fb back to scan out).
//!
//! The whole card is virtual and fixed: exactly one of each KMS object, the
//! dumb buffers are page-backed like a memfd (so `MAP_DUMB` + `mmap` aliases
//! the real frames, with the same per-mapping reference discipline so a
//! unmapping process cannot free a buffer another still owns). A page flip
//! is applied by *copying the framebuffer into the GPU's scanout buffer* -
//! row by row, because the dumb buffer's pitch is 64-byte aligned while the
//! scanout's is `width * 4` - and then asking the render task to flush, so
//! what the compositor drew actually reaches the host window. The ioctl
//! numbers and struct layouts follow the Linux UAPI exactly (`drm.h`,
//! `drm_mode.h`), which is what libdrm compiles against.
//!
//! `/dev/dri/renderD128` is the same card state under the render node's
//! name: GBM opens the render node to allocate (a client is not the master,
//! so libdrm's convention is to keep allocation off `card0`), and a teaching
//! kernel has no permission boundary to draw between the two. Both nodes
//! ioctl one shared `DrmState`.

/// The card's parts, split from the one file this was: `abi` is the Linux UAPI
/// vocabulary (object ids, property ids, ioctl numbers, caps), `state` owns the
/// single `DrmState` behind both nodes, `ioctl` decodes and dispatches a
/// request, `props` answers the KMS enumeration and property doors (and the
/// atomic commit), `fb` owns buffers and framebuffers, `flip` presents them,
/// `sync` is sync objects and fences, `master` is master/magic/lease authority,
/// `virtgpu` is the 3D family over the published card, `card` is the two
/// device nodes themselves. Cross-child access is crate-visible; the outside
/// world keeps importing through `drivers::drm` (`mod.rs` re-exports below).
mod abi;
mod card;
mod fb;
mod flip;
mod ioctl;
#[cfg(all(target_os = "none", feature = "selftest"))]
pub mod kunit;
mod master;
mod props;
mod state;
mod sync;
mod virtgpu;

pub use card::DrmCard;
pub use master::frame_landed;
pub use master::{
    card_grabbed_by_current, close_at_exit, grant_and_resent, is_master, master_closed,
    master_held, re_present_now, revoke_now, revoked,
};
pub use props::client_scanout;
pub use props::display_changed;
pub use virtgpu::publish_card;

/// Serve a DRM ioctl on the card a compositor holds - the dispatch lives in the
/// `ioctl` child; this is the name outside `drivers::drm` calls.
pub use ioctl::drm_request as drm_ioctl;
