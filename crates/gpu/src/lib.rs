#![no_std]

//! The GPU's pure, host-testable half. Each submodule holds a decision
//! table or an encoder that a host test can reach; the driver bodies that
//! need a device stay in the kernel's `drivers`.
//!
//! Its own crate as of Phase 11 Step E, for the reason every leaf there was
//! split out: the body of the work is arithmetic over numbers a real card
//! produced (`PLAN.md` §7.4), and everything that reads it is machine-only. A
//! decision that can only be exercised by booting is a decision nobody checks
//! - which is how a KMS driver came to advertise a property with no value and a
//! sync object came to answer as "ready" for a fence that did not exist.
//!
//! The one thing in here that was *not* pure was the wake: a sync object's
//! waiter is a parked scheduler task, and [`sync::Waker`] is now how that is
//! said without naming a scheduler. `drivers::drm` injects the kernel's
//! implementation; a host test injects one that records. That also made the
//! wake itself testable off the machine, which it never was.

extern crate alloc;

/// The virtio-gpu protocol: the control commands and their shapes, as the wire
/// spells them. Not target-gated, and it has to be: the driver beside it is
/// never compiled off the machine, and what that driver and a real QEMU must
/// agree on is exactly the kind of fact - a command number, a field offset, a
/// struct size - that an agreement with itself cannot check. `drivers/drm.rs`
/// records what happens when a number is pinned only against the caller that
/// sends it.
pub mod abi;
/// The two DRM blobs a KMS driver publishes as bytes: an EDID, and the
/// `IN_FORMATS` list of which format-and-modifier pairs a plane scans out.
/// Ungated because both are pure encoders and the bytes are what a reader
/// parses - `drivers/drm.rs` holds the card's state and talks to the GPU, so
/// nothing inside it can be checked off the machine, and an EDID one field out
/// of place is a display no compositor can name.
pub mod blob;
/// The DRM cursor's composition: an `ARGB8888` image blended over the `BGRA`
/// scanout at the pointer, in the one place a host test can reach it. The
/// driver next to it is never compiled off the machine, and this is the only
/// arithmetic on the presentation path - a 64x64 blend clipped at four edges,
/// where being wrong at one edge and right at three is the normal way it fails.
pub mod cursor;
/// The DRM sync objects' state machine: what a `SYNCOBJ_*` request answers, in
/// the one place a host test can reach it. The driver next to it is never
/// compiled off the machine, and this is the family where the authorities
/// disagree - the upstream sources read a sync object with no fence as already
/// signaled and the real card refuses it (`PLAN.md` §7.4) - so the decisions are
/// written down here with a test per rule instead of being buried in a `match`
/// only a boot can exercise. It is also where the crate's one seam lives:
/// [`sync::Waker`].
pub mod sync;
/// The `DRM_VIRTGPU_*` family's answers on a card with no 3D behind it. Ungated
/// like its three siblings, and for the same reason: what a caller hears for
/// each request is a table of decisions about numbers, measured on a real
/// Linux, and a table is worth a host test rather than a boot. The requests that
/// need a resource to exist are absent here on purpose - their honest answer
/// needs the device, and the module says so.
pub mod virtgpu;
