//! The `DRM_VIRTGPU_*` family: what a card with no 3D behind it answers, and why
//! each answer is Linux's.
//!
//! The family is how Mesa's virgl driver and every Vulkan-on-virtio-gpu stack
//! reach the device: `GETPARAM` to learn what the card can do, `EXECBUFFER` to
//! submit a command stream, `TRANSFER_*` to move pixels, `RESOURCE_CREATE` to
//! allocate. This card has none of that yet - the 3D control queue and the
//! resource it would allocate are the work `PLAN.md` §7.4 names - and the point
//! of this module is the other half of that sentence: what a caller should hear
//! *in the meantime*.
//!
//! The answer is not "nothing", which is what the card said before: every one of
//! the eleven requests fell to the DRM tail's `ENOTTY`, and Linux answers that
//! code for a request its core does not know, never for one a registered driver
//! declines. The measurement the answers below are taken from is in
//! `user/staging/virtgpu-probe/`: a real Linux 6.12 on a `virtio-gpu-pci` card
//! with no virgl behind it, every request issued and every errno recorded.
//!
//! What that measurement says, and what this module encodes, is that **the
//! refusals are per-request facts rather than one gate on the family**. A card
//! without 3D still *succeeds* at `TRANSFER_TO_HOST`, `MAP` and `WAIT`, refuses
//! `EXECBUFFER`, `TRANSFER_FROM_HOST` and `GET_CAPS` with `ENOSYS`, and refuses
//! `CONTEXT_INIT`, `RESOURCE_CREATE_BLOB` and a 3D `RESOURCE_CREATE` with
//! `EINVAL` - three different answers inside one family, and a single errno for
//! the whole of it would send a caller down a path it cannot recover from. That
//! is §4's wrong-shape failure in its plainest form, and it is why this is a
//! table rather than a constant.
//!
//! # What is *not* here, and why that is deliberate
//!
//! Four requests name a resource handle - `RESOURCE_INFO`, `MAP`, `WAIT` and
//! `TRANSFER_TO_HOST` - and on a real card all four *succeed*. They are absent
//! because their honest answer needs a resource to exist, and this card cannot
//! make one: a resource is a device-side allocation told to the device over the
//! control queue, and the queue belongs to the render task. Answering `ENOENT`
//! for every handle was considered and rejected: this card's handle table holds
//! *dumb buffers*, and on Linux a dumb buffer **is** a GEM object that
//! `RESOURCE_INFO` answers for, so "no handle is a resource" would be a dialect
//! of this kernel's rather than Linux's - §4 rule 3. They land with the resource
//! path, and until then they keep the tail's `ENOTTY`, which is at least the
//! same wrong answer they have always had rather than a new one.
//!
//! Nothing here is `cfg`-gated, for the same reason `drm_sync.rs` and
//! `drm_blob.rs` are not: these are decisions about numbers, and a decision
//! about a number is worth a host test rather than a boot.

use crate::err::{EINVAL, ENOSYS};

/// `VIRTGPU_PARAM_3D_FEATURES`: whether the card can run a 3D context.
pub const PARAM_3D_FEATURES: u64 = 1;
/// `VIRTGPU_PARAM_CAPSET_QUERY_FIX`: whether `GET_CAPS` has the "report the
/// capset's real size" fix. Linux answers `1` unconditionally, and it is the one
/// parameter that is a statement about the *query* rather than about hardware -
/// so this card answers it exactly as a real one does, 3D or not (measured).
pub const PARAM_CAPSET_QUERY_FIX: u64 = 2;
/// `VIRTGPU_PARAM_RESOURCE_BLOB`: whether blob resources can be allocated.
pub const PARAM_RESOURCE_BLOB: u64 = 3;
/// `VIRTGPU_PARAM_HOST_VISIBLE`: whether a host-visible blob can be mapped.
pub const PARAM_HOST_VISIBLE: u64 = 4;
/// `VIRTGPU_PARAM_CROSS_DEVICE`: whether a resource can be shared with another
/// virtio device by UUID.
pub const PARAM_CROSS_DEVICE: u64 = 5;
/// `VIRTGPU_PARAM_CONTEXT_INIT`: whether a context can be created explicitly,
/// with several command rings.
pub const PARAM_CONTEXT_INIT: u64 = 6;
/// `VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs`: a *bitmask* of the capsets the device
/// offers - bit `n` set means capset `n` (VIRGL is 1, VIRGL2 is 2).
pub const PARAM_SUPPORTED_CAPSET_IDS: u64 = 7;
/// `VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME`: whether a context can be named from
/// userspace. Gated on the same feature as `CONTEXT_INIT` on a real card.
pub const PARAM_EXPLICIT_DEBUG_NAME: u64 = 8;

/// The value of `GETPARAM(param)`, or the errno the request is refused with.
///
/// Every value here is a property of *this* kernel, not of the device on the
/// other side of the transport, and that is the honest thing for them to be:
/// `3D_FEATURES` reports whether a 3D context can be run, and this kernel
/// negotiates no `VIRGL` feature and has no command that could create one. A
/// machine booted on the GL card therefore answers `0` here - not because the
/// device cannot, but because this driver cannot, which is exactly the question
/// a caller is asking and the reason `PLAN.md` §7.4's remaining work is a kernel
/// change rather than a machine one.
///
/// A parameter outside the table is `EINVAL` (measured: `0`, `9` and `100` all
/// answer it on a real card). `BLOB_ALIGNMENT` is 9 in newer headers and is
/// deliberately *not* answered: this card has no blob resources to align.
pub fn param(param: u64) -> Result<u64, isize> {
    match param {
        PARAM_3D_FEATURES => Ok(0),
        PARAM_CAPSET_QUERY_FIX => Ok(1),
        PARAM_RESOURCE_BLOB => Ok(0),
        PARAM_HOST_VISIBLE => Ok(0),
        PARAM_CROSS_DEVICE => Ok(0),
        PARAM_CONTEXT_INIT => Ok(0),
        PARAM_SUPPORTED_CAPSET_IDS => Ok(0),
        PARAM_EXPLICIT_DEBUG_NAME => Ok(0),
        _ => Err(EINVAL),
    }
}

/// What `GET_CAPS` answers: `ENOSYS`, which is Linux's answer on a card whose
/// device reported no cap sets at all (measured).
///
/// It is this card's answer even on a GL machine, where the device *would* offer
/// VIRGL and VIRGL2, and the reason is the same as `3D_FEATURES`'s: fetching a
/// capset is `GET_CAPSET` on the control queue, and a capset this kernel cannot
/// fetch is one it cannot hand over. Answering `ENOSYS` rather than an empty
/// buffer is the difference between "there is nothing here for you" and a
/// capability word of zeroes a caller would read as a real answer.
pub const CAPS: isize = ENOSYS;

/// `EXECBUFFER` refuses `ENOSYS`: submitting a command stream is the 3D path,
/// and this kernel has no context to submit it to. Measured as a real card's
/// answer when no virgl is negotiated.
pub const EXECBUFFER: isize = ENOSYS;

/// `TRANSFER_FROM_HOST` refuses `ENOSYS`, and `TRANSFER_TO_HOST` does *not* -
/// the asymmetry is Linux's and was the least guessable thing in the
/// measurement: a card without 3D transfers *to* the host through the 2D
/// command, so the request succeeds, while the reverse direction has only the
/// 3D command and is refused. This card implements neither yet, so both are
/// named here rather than left to look symmetric.
pub const TRANSFER_FROM_HOST: isize = ENOSYS;

/// `CONTEXT_INIT` refuses `EINVAL` - not `ENOSYS`, which is what a guess would
/// pick. Measured: the driver's gate is `!has_context_init || !has_virgl_3d`
/// and it answers `EINVAL` for both.
pub const CONTEXT_INIT: isize = EINVAL;

/// `RESOURCE_CREATE_BLOB` refuses `EINVAL`: the driver's blob check answers it
/// when the device did not offer `VIRTIO_GPU_F_RESOURCE_BLOB`, which a card
/// without 3D does not.
pub const RESOURCE_CREATE_BLOB: isize = EINVAL;

/// The `RESOURCE_CREATE` request is refused `ENOSYS` *after* its arguments pass
/// the validation below.
///
/// This is the one answer here that is not a measured Linux answer, and it is
/// named as such rather than hidden: on a real card a valid 2D `RESOURCE_CREATE`
/// **succeeds**, because the driver tells the device to make a resource and
/// attaches the caller's pages to it. This card cannot: the control queue that
/// would carry that command belongs to the render task, and reaching it from an
/// ioctl is the mailbox `PLAN.md` §7.4 describes. `ENOSYS` - "this card does not
/// do that" - is the honest refusal §4 asks for; a fabricated handle would be
/// the false success that same section calls the worst thing this codebase can
/// do, because the caller would build on it.
pub const RESOURCE_CREATE_UNSUPPORTED: isize = ENOSYS;

/// The `target` a 2D resource is created with. The only value a card without 3D
/// accepts, and the one its own framebuffer uses.
pub const TARGET_2D: u32 = 2;

/// Whether `RESOURCE_CREATE`'s arguments are ones *any* card would accept.
///
/// The validation is the driver's non-virgl branch, measured rather than
/// inferred: without 3D it refuses `target != 2`, `depth > 1`, `nr_samples > 1`,
/// `last_level > 1` and `array_size > 1`, each with `EINVAL`. Checking these
/// before refusing with `ENOSYS` is what keeps the two answers apart - a caller
/// whose arguments are wrong hears what is wrong with them, and one whose
/// arguments are fine hears that this card cannot allocate, which is a different
/// problem with a different remedy.
pub fn resource_create_args_ok(
    target: u32,
    depth: u32,
    nr_samples: u32,
    last_level: u32,
    array_size: u32,
) -> Result<(), isize> {
    if target != TARGET_2D
        || depth > 1
        || nr_samples > 1
        || last_level > 1
        || array_size > 1
    {
        return Err(EINVAL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parameter table is the measured one, value for value.
    ///
    /// Written against the literals the instrument printed rather than against
    /// the constants above, because a table checked against its own names agrees
    /// with any numbering at all - the mistake `drivers/drm.rs` records for the
    /// lease family.
    #[test]
    fn the_parameter_table_is_the_measured_one() {
        assert_eq!(param(1), Ok(0)); // 3D_FEATURES: this kernel negotiates no VIRGL
        assert_eq!(param(2), Ok(1)); // CAPSET_QUERY_FIX: unconditional, measured
        assert_eq!(param(3), Ok(0)); // RESOURCE_BLOB
        assert_eq!(param(4), Ok(0)); // HOST_VISIBLE
        assert_eq!(param(5), Ok(0)); // CROSS_DEVICE
        assert_eq!(param(6), Ok(0)); // CONTEXT_INIT
        assert_eq!(param(7), Ok(0)); // SUPPORTED_CAPSET_IDs: the mask, empty here
        assert_eq!(param(8), Ok(0)); // EXPLICIT_DEBUG_NAME
    }

    /// A parameter the card does not have is `EINVAL`, including the ones a
    /// newer header adds.
    #[test]
    fn an_unknown_parameter_is_einval() {
        for outside in [0, 9, 100, u64::MAX] {
            assert_eq!(param(outside), Err(EINVAL), "param {outside}");
        }
        // 9 is `BLOB_ALIGNMENT` in a newer header, and it is refused because
        // this card has no blobs to align - not because the number is unknown.
        assert_eq!(PARAM_EXPLICIT_DEBUG_NAME + 1, 9);
    }

    /// The non-virgl validation is the driver's, field by field.
    #[test]
    fn a_2d_resource_with_single_layer_arguments_is_accepted_for_validation() {
        assert_eq!(resource_create_args_ok(2, 1, 1, 1, 1), Ok(()));
        // Each field on its own is enough to refuse: a card without 3D has no
        // arrayed, multisampled or mip-mapped resources.
        for (target, depth, samples, level, array) in [
            (4, 1, 1, 1, 1), // the 3D target
            (2, 2, 1, 1, 1), // depth
            (2, 1, 2, 1, 1), // nr_samples
            (2, 1, 1, 2, 1), // last_level
            (2, 1, 1, 1, 2), // array_size
            (0, 0, 0, 0, 0),
        ] {
            assert_eq!(
                resource_create_args_ok(target, depth, samples, level, array),
                Err(EINVAL),
                "target={target} depth={depth} samples={samples} level={level} array={array}"
            );
        }
    }

    /// The three distinct answers inside one family, kept distinct. A change
    /// that collapsed them to one code would pass every other test here.
    #[test]
    fn the_refusals_are_not_one_gate() {
        assert_eq!(CAPS, ENOSYS);
        assert_eq!(EXECBUFFER, ENOSYS);
        assert_eq!(TRANSFER_FROM_HOST, ENOSYS);
        assert_eq!(CONTEXT_INIT, EINVAL);
        assert_eq!(RESOURCE_CREATE_BLOB, EINVAL);
        assert_eq!(RESOURCE_CREATE_UNSUPPORTED, ENOSYS);
        // The two families of answer are genuinely different codes, which is the
        // whole claim of the table above.
        assert_ne!(CAPS, CONTEXT_INIT);
    }
}
