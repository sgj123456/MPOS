//! The `DRM_VIRTGPU_*` family: what this card answers, card by card, and why
//! each answer is Linux's.
//!
//! The family is how Mesa's virgl driver and every Vulkan-on-virtio-gpu stack
//! reach the device: `GETPARAM` to learn what the card can do, `EXECBUFFER` to
//! submit a command stream, `TRANSFER_*` to move pixels, `RESOURCE_CREATE` to
//! allocate. There are two machines to answer for - one whose GPU is a
//! `virtio-gpu-pci` with no 3D behind it, and one whose GPU is a
//! `virtio-gpu-gl-pci` with virgl - and the point of this module is that the
//! answers are a *function of which*, not a constant.
//!
//! [`Card`] is that function's input and [`param`] is its most visible arm: the
//! same request answers `0` and `1` on the two machines. The rest of the family
//! splits the same way - a card with no cap sets answers `GET_CAPS` `ENOSYS`
//! while a GL card serves its sets out of [`Capset`] - and the handlers that
//! make that split live in `drivers/drm.rs`, which is where the resource table
//! and the per-open-file context are too.
//!
//! The answers are not "nothing", which is what the card said before: every one
//! of the eleven requests fell to the DRM tail's `ENOTTY`, and Linux answers that
//! code for a request its core does not know, never for one a registered driver
//! declines. The measurement everything here is taken from is in
//! `user/staging/virtgpu-probe/`: a real Linux 6.12 booted twice on this host,
//! once per card shape, every request issued and every errno recorded.
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
//! # What is served, and the one field that is not Linux's
//!
//! All eleven requests are dispatched now. `GETPARAM` and the refusals below are
//! this module; `RESOURCE_CREATE` in `drivers/drm.rs` allocates a buffer and has
//! the render task tell the device to make a resource of it, and `RESOURCE_INFO`,
//! `MAP`, `WAIT` and `TRANSFER_TO_HOST` answer for the handles that come back.
//! The measurement above is why the refusals are per-request, and it is also why
//! `MAP` and `WAIT` *succeed* here rather than being refused with the rest: a
//! card without 3D answers them, so refusing them would be a new wrong answer
//! rather than the old one.
//!
//! Three things are this card's rather than Linux's, and each is named where it
//! happens rather than left to be found - two of them closed since this header
//! was written:
//!
//! 1. **`TRANSFER_FROM_HOST` is `ENOSYS` here and on Linux alike** - that one is
//!    the measurement, not a divergence: the only command that reads a resource
//!    back is the 3D transfer.
//! 2. **`RESOURCE_INFO`'s `res_handle` was 0 for a plain dumb buffer.** On Linux
//!    a dumb buffer *is* a device resource and the field names it; here one is
//!    the card's own pages and the device was never told about it, so the field
//!    said the protocol's own "no resource" (`SET_SCANOUT` spells it the same
//!    way). **Closed**: `drivers/drm.rs`'s `back_dumb_with_a_resource` makes the
//!    resource on the first request that addresses the handle as one, so a
//!    client reads Linux's answer and a client that never asks - the console,
//!    weston and the fbdev node all allocate through `CREATE_DUMB` and none of
//!    them asks - pays neither the eager backing nor the device round trip that
//!    deferred the fix.
//! 3. **`TRANSFER_TO_HOST` on a plain dumb buffer was `EINVAL`** where Linux
//!    completes it - same cause, and it was answered as "this command does not
//!    apply to that handle" rather than `0`, because a transfer that did nothing
//!    while reporting success is the false success this repository forbids.
//!    **Closed with 2**: the first transfer against such a handle makes the
//!    resource and then takes the 2D path, which is the copy Linux makes.
//!
//! What is left is a *refusal* rather than a different answer: a transfer whose
//! rectangle does not fit the backing the device was given is refused where
//! Linux would read past it, and `RESOURCE_CREATE_BLOB` is `EINVAL` on both
//! shapes because there are no blobs here. `NOTES.md` §7.4 keeps the measurement
//! of all of them.
//!
//! Nothing here is `cfg`-gated, for the same reason `drm_sync.rs` and
//! `drm_blob.rs` are not: these are decisions about numbers, and a decision
//! about a number is worth a host test rather than a boot.

use alloc::vec::Vec;

use mpos_err::{EINVAL, ENOSYS};

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

/// What the device behind the transport turned out to be, as far as the
/// requests in this module are concerned.
///
/// One value rather than a set of loose flags, because every answer below is a
/// statement about the *card* and the measurement is what shows they move
/// together: the machine that negotiated no `VIRGL` answers `0` to
/// `3D_FEATURES`, `0` to `CONTEXT_INIT`, `0` for the cap set mask, and refuses
/// every 3D request with the same errnos. It is built once, from the feature
/// word the transport actually agreed, and does not change afterwards - a
/// capability that moved under a running ioctl path would be a worse answer
/// than a stale one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Card {
    /// Whether `VIRGL` was **negotiated** - asked for and agreed.
    ///
    /// Negotiated rather than merely offered, and the difference is the whole
    /// point of this field: a device that offered 3D in front of a driver that
    /// never asked for it has no context and no command to make one, and it is
    /// the driver's answer a caller is asking about. Linux's `has_virgl_3d` is
    /// the same fact under another name.
    pub virgl: bool,
    /// Whether the device takes `CTX_CREATE` explicitly, so a context can be
    /// made with its cap set named rather than implied by the first command.
    pub context_init: bool,
    /// The device's cap sets as the bitmask `GETPARAM(7)` reports: bit `n` set
    /// means cap set `n` is offered. A mask rather than a list because that is
    /// what the parameter *is*, and both questions a caller asks of it - "is
    /// set 1 there" and "which are there" - are tests of one bit.
    pub capset_ids: u32,
    /// Whether the device has a cursor queue - the fact that decides whether the
    /// DRM layer hands the pointer to the host (`UPDATE_CURSOR`/`MOVE_CURSOR`) or
    /// blends it into the scanout itself. Read from the negotiated device at
    /// bring-up, not from a feature bit: virtio-gpu's cursor support is implied by
    /// the second queue existing, and there is no `F_CURSOR` in the feature word.
    /// `false` is both "no queue" and "no card", which is why [`Card::NONE`] says
    /// so rather than leaving it undecided.
    pub cursor_q: bool,
}

impl Card {
    /// The card a machine whose GPU never came up has: no 3D, no context, no
    /// cap sets.
    ///
    /// Every answer here reduces to this when there is nothing to ask, and it
    /// is what the DRM layer passes until a driver has published a real card. A
    /// device that failed to initialise must answer a caller the way a card
    /// with no 3D does, not the way a card that broke does - the difference
    /// between "there is nothing here for you" and a capability the caller
    /// reads as real is the whole reason this module is a table.
    pub const NONE: Card = Card {
        virgl: false,
        context_init: false,
        capset_ids: 0,
        cursor_q: false,
    };
}

/// The value of `GETPARAM(param)`, or the errno the request is refused with.
///
/// Every value answers a question about *this driver*, which is why most of
/// them are not a read of the device: `3D_FEATURES` reports whether a 3D
/// context can be run, and a device that offers 3D to a driver with no
/// `CTX_CREATE` behind it would be answered `1` by a table that read the device
/// and `0` here. Linux's own table says the same thing in its own terms, and
/// the measured card is where the values below come from - on a machine with no
/// 3D, `GETPARAM(1)`, `(6)` and `(7)` answer `0` and `(2)` answers `1`.
///
/// Three answers are `0` whatever the device offers, and that is deliberate
/// rather than unfinished: `RESOURCE_BLOB`, `HOST_VISIBLE` and `CROSS_DEVICE`
/// report blob resources, host-visible memory and cross-device sharing, and
/// this kernel has no command for any of the three. A `1` here would advertise
/// a path with nothing behind it. The measured card agrees (`0`, `0`, `0`) for
/// its own reason - QEMU offers no blob feature on this machine - and
/// `NOTES.md` §7.4 records that blobs are out of the work.
///
/// A parameter outside the table is `EINVAL` (measured: `0`, `9` and `100` all
/// answer it on a real card). `BLOB_ALIGNMENT` is 9 in newer headers and is
/// deliberately *not* answered: there are no blobs here to align.
pub fn param(param: u64, card: &Card) -> Result<u64, isize> {
    match param {
        PARAM_3D_FEATURES => Ok(card.virgl as u64),
        PARAM_CAPSET_QUERY_FIX => Ok(1),
        PARAM_RESOURCE_BLOB => Ok(0),
        PARAM_HOST_VISIBLE => Ok(0),
        PARAM_CROSS_DEVICE => Ok(0),
        PARAM_CONTEXT_INIT => Ok(card.context_init as u64),
        PARAM_SUPPORTED_CAPSET_IDS => Ok(card.capset_ids as u64),
        PARAM_EXPLICIT_DEBUG_NAME => Ok(card.context_init as u64),
        _ => Err(EINVAL),
    }
}

/// How many bytes of a cap set `GET_CAPS` may copy out, or the errno it is
/// refused with.
///
/// Linux's rule, and the part of it worth naming is that a **partial copy is
/// legal**: `size = min(args->size, host_caps_size)`, then a copy of exactly
/// `size` bytes. A caller whose buffer is smaller than the cap set gets the
/// front of it and a success - not a truncation error and not the whole set
/// spilling past its allocation. A caller that asked for nothing is `EINVAL`
/// before either side is looked at, which is the other half of the same rule.
///
/// `host_max_size` is what `GET_CAPSET_INFO` reported for the set being
/// fetched, so this is the one place the two commands' answers meet.
pub fn caps_may_copy(caller_size: u32, host_max_size: u32) -> Result<usize, isize> {
    if caller_size == 0 {
        return Err(EINVAL);
    }
    Ok(caller_size.min(host_max_size) as usize)
}

/// One cap set, described and held.
///
/// The three metadata fields are `GET_CAPSET_INFO`'s answer and `bytes` is the
/// set itself, fetched once at bring-up rather than per request. That is Linux's
/// shape too - it keeps a `cap_cache` list and `virtio_gpu_get_caps_ioctl` walks
/// it before it will talk to the hardware - and the reason is the same here as
/// there: fetching is a control-queue round trip, and `GET_CAPS` is a request a
/// caller may make on every startup.
///
/// The metadata is kept beside the bytes rather than derived from them because
/// the two answer different questions: `max_size` is what the *device* promised
/// (and what bounds a fetch), while `bytes.len()` is what it actually sent. A
/// device that sent short is one this module refuses to have cached at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capset {
    pub id: u32,
    pub max_version: u32,
    pub max_size: u32,
    pub bytes: Vec<u8>,
}

/// The `GETPARAM(7)` mask for a table of cap sets: bit `n` set means set `n`.
///
/// The one place the family's two spellings of the same fact meet. Linux keeps
/// `capset_id_mask` beside its cap set array for exactly this reason, and the
/// measured GL card is where they agree: two sets with ids 1 and 2 are the mask
/// `6`, which is what `GETPARAM(7)` answered.
///
/// An id of 32 or more has no bit and is dropped rather than shifted - the
/// parameter is a `u32` mask, so a set numbered past its width is one no caller
/// could ever ask for. Nothing measured goes near it; a device that claimed one
/// would be describing a set its own parameter cannot name.
pub fn table_mask(capsets: &[Capset]) -> u32 {
    let mut mask = 0;
    for capset in capsets {
        if capset.id < 32 {
            mask |= 1 << capset.id;
        }
    }
    mask
}

/// What `GET_CAPS` answers on a card with no cap sets at all: `ENOSYS`.
///
/// Linux's gate is `num_capsets == 0`, and the measurement agrees - a
/// `virtio-gpu-pci` card answers `ENOSYS` to every set asked for. It is **not**
/// the family's one answer: a GL card has two sets and serves them, which is
/// what [`Capset`] and [`caps_may_copy`] are for, and the handler that serves
/// them is `drivers/drm.rs`. `ENOSYS` rather than an empty buffer is the
/// difference between "there is nothing here for you" and a capability word of
/// zeroes a caller would read as a real answer.
pub const CAPS: isize = ENOSYS;

/// `EXECBUFFER` on a card with no 3D: `ENOSYS`.
///
/// Submitting a command stream is the 3D path, and a card that negotiated no
/// `VIRGL` has no context to submit it to - which is the measured answer rather
/// than a guess. Where there *is* a context the request is served, and
/// `drivers/drm.rs` is where the two are told apart.
pub const EXECBUFFER: isize = ENOSYS;

/// `TRANSFER_FROM_HOST` on a card with no 3D: `ENOSYS`.
///
/// `TRANSFER_TO_HOST` does **not** refuse, and the asymmetry is Linux's and was
/// the least guessable thing in the measurement: a card without 3D transfers
/// *to* the host through the 2D command, so the request succeeds, while the
/// reverse direction has only the 3D command and is refused. Writing the pair
/// down rather than leaving them to look symmetric is the point.
pub const TRANSFER_FROM_HOST: isize = ENOSYS;

/// `CONTEXT_INIT` on a card with no 3D: `EINVAL`, not `ENOSYS`.
///
/// Measured, and it is what a guess gets wrong: the driver's gate is
/// `!has_context_init || !has_virgl_3d` and both halves answer `EINVAL`. With
/// both features the request is served - and on a card whose context is made
/// lazily it answers `EEXIST` the second time, which is the measurement's most
/// counter-intuitive line.
pub const CONTEXT_INIT: isize = EINVAL;

/// `RESOURCE_CREATE_BLOB` on a card with no blobs: `EINVAL`.
///
/// Measured on **both** cards, GL included: the driver's blob check answers
/// `EINVAL` when the device did not offer `VIRTIO_GPU_F_RESOURCE_BLOB`, and the
/// GL card this work was measured against does not. Since this kernel has no
/// blob command either, the answer is the same wherever the request arrives and
/// there is no second case to write.
pub const RESOURCE_CREATE_BLOB: isize = EINVAL;

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
    if target != TARGET_2D || depth > 1 || nr_samples > 1 || last_level > 1 || array_size > 1 {
        return Err(EINVAL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The no-3D card's answers are the measured ones, value for value.
    ///
    /// Written against the literals the instrument printed rather than against
    /// the constants above, because a table checked against its own names agrees
    /// with any numbering at all - the mistake `drivers/drm.rs` records for the
    /// lease family.
    #[test]
    fn a_card_without_3d_answers_the_measured_row() {
        let card = Card {
            virgl: false,
            context_init: false,
            capset_ids: 0,
            cursor_q: false,
        };
        assert_eq!(param(1, &card), Ok(0)); // 3D_FEATURES
        assert_eq!(param(2, &card), Ok(1)); // CAPSET_QUERY_FIX: unconditional, measured
        assert_eq!(param(3, &card), Ok(0)); // RESOURCE_BLOB
        assert_eq!(param(4, &card), Ok(0)); // HOST_VISIBLE
        assert_eq!(param(5, &card), Ok(0)); // CROSS_DEVICE
        assert_eq!(param(6, &card), Ok(0)); // CONTEXT_INIT
        assert_eq!(param(7, &card), Ok(0)); // SUPPORTED_CAPSET_IDs: the mask, empty here
        assert_eq!(param(8, &card), Ok(0)); // EXPLICIT_DEBUG_NAME
        // The fallback the DRM layer passes until a driver publishes a real
        // card is this same machine, rather than something merely similar.
        assert_eq!(Card::NONE, card);
    }

    /// The GL card's answers are the measured ones, and the three that do not
    /// move with it are the point of the second half.
    #[test]
    fn a_virgl_card_answers_the_measured_row() {
        let card = Card {
            virgl: true,
            context_init: true,
            capset_ids: 6,
            cursor_q: false,
        };
        assert_eq!(param(1, &card), Ok(1)); // 3D_FEATURES
        assert_eq!(param(2, &card), Ok(1)); // CAPSET_QUERY_FIX
        assert_eq!(param(6, &card), Ok(1)); // CONTEXT_INIT
        assert_eq!(param(7, &card), Ok(6)); // the mask: cap sets 1 and 2
        assert_eq!(param(8, &card), Ok(1)); // follows CONTEXT_INIT, as Linux's does
        // Blob resources, host-visible memory and cross-device sharing are
        // answered `0` whatever the device offers, because the commands behind
        // them do not exist here and a `1` would advertise a path with nothing
        // behind it. These three are the only values in the table that do not
        // move with the card, and that is the false-capability rule rather than
        // an omission.
        for blob in [3, 4, 5] {
            assert_eq!(param(blob, &card), Ok(0), "param {blob}");
        }
    }

    /// A parameter the card does not have is `EINVAL`, including the ones a
    /// newer header adds - and on either card, since the table's shape is the
    /// card's and its extent is not.
    #[test]
    fn an_unknown_parameter_is_einval() {
        let cards = [
            Card::NONE,
            Card {
                virgl: true,
                context_init: true,
                capset_ids: 6,
                cursor_q: false,
            },
        ];
        for card in &cards {
            for outside in [0, 9, 100, u64::MAX] {
                assert_eq!(param(outside, card), Err(EINVAL), "param {outside}");
            }
        }
        // 9 is `BLOB_ALIGNMENT` in a newer header, and it is refused because
        // this card has no blobs to align - not because the number is unknown.
        assert_eq!(PARAM_EXPLICIT_DEBUG_NAME + 1, 9);
    }

    /// The `GET_CAPS` size rule, including the half a guess gets wrong: a copy
    /// smaller than the cap set is a **success**, not a truncation error.
    #[test]
    fn a_capset_copy_is_the_smaller_of_the_two_sizes() {
        // The measured pair on the GL card: VIRGL is 308 bytes, VIRGL2 1408.
        assert_eq!(caps_may_copy(4096, 308), Ok(308));
        assert_eq!(caps_may_copy(4096, 1408), Ok(1408));
        assert_eq!(caps_may_copy(100, 308), Ok(100));
        assert_eq!(caps_may_copy(308, 308), Ok(308));
        // A caller that asked for nothing is refused before either size is
        // compared - Linux's order, and the reason `0` is not "copy nothing".
        assert_eq!(caps_may_copy(0, 308), Err(EINVAL));
        // A set the device reports as empty copies nothing and still succeeds:
        // the `0` below is the caller's own size coming back, not an error.
        assert_eq!(caps_may_copy(16, 0), Ok(0));
    }

    /// The mask and the table are two spellings of one fact, and the measured
    /// card is where they agree.
    #[test]
    fn the_capset_mask_is_the_table() {
        let capsets = |ids: &[u32]| -> Vec<Capset> {
            ids.iter()
                .map(|&id| Capset {
                    id,
                    max_version: 1,
                    max_size: 308,
                    bytes: Vec::new(),
                })
                .collect()
        };
        assert_eq!(table_mask(&capsets(&[])), 0);
        // The measured GL card: VIRGL and VIRGL2, which `GETPARAM(7)` answered
        // as `6`. If these two ever disagree the card is describing a cap set
        // its own parameter cannot name.
        assert_eq!(table_mask(&capsets(&[1, 2])), 6);
        assert_eq!(table_mask(&capsets(&[1])), 2);
        assert_eq!(table_mask(&capsets(&[2])), 4);
        // Order does not matter and neither does a repeat - a device that
        // listed a set twice offers one set.
        assert_eq!(table_mask(&capsets(&[2, 1, 1])), 6);
        // An id with no bit in a `u32` is dropped rather than shifted, which is
        // the difference between a mask and undefined behaviour.
        assert_eq!(table_mask(&capsets(&[1, 32, 64])), 2);
        assert_eq!(table_mask(&capsets(&[31])), 1 << 31);
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
        // The two families of answer are genuinely different codes, which is the
        // whole claim of the table above. `TRANSFER_TO_HOST` and `MAP` are not
        // here because they are *served* - a card without 3D succeeds at both,
        // and `RESOURCE_CREATE` now does too.
        assert_ne!(CAPS, CONTEXT_INIT);
    }
}
