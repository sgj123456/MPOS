#![no_main]

//! The `GET_CAPSET` reply: the one virtio-gpu answer whose length the *device*
//! decides.
//!
//! `struct virtio_gpu_resp_capset` ends in a flexible array (`__u8
//! capset_data[]`), so the reply carries no length of its own - the only bound
//! on it is the buffer the driver allocated, and the driver sized that from the
//! `capset_max_size` the device itself reported a round trip earlier.
//! `virtgpu_abi::resp_capset_payload` is what names the payload, and what it
//! has to survive is a device that answered with fewer bytes than it owed, a
//! header split across the end of the buffer, or an error code where a cap set
//! belongs.
//!
//! This is the family's only parser, and it is a parser for exactly that
//! reason: the length is the device's. `SUBMIT_3D`'s command stream is the
//! protocol's other length and is deliberately **not** here - those bytes come
//! from the caller and are forwarded to the device untouched, the way Linux
//! hands them over with `vmemdup_user`, so nothing on this side walks a length
//! the caller supplied. `src/virtgpu_abi.rs` says the same in its header.

use libfuzzer_sys::fuzz_target;
use mpos::virtgpu_abi::{self, CtrlHeader, RESP_ERR_INVALID_PARAMETER, RESP_OK_CAPSET};

/// The reader's contract, asserted directly and on every input.
///
/// Four specific bytes in the first word are not something a byte mutator
/// finds by chance, and worse: a reader that regressed to always answering
/// `None` would leave every assertion below vacuous while the fuzzer reported
/// a clean run - the failure mode where a fuzz target stops testing anything
/// and nobody notices. So the two cases the assertions below depend on being
/// reachable are pinned here, at the top, before the corpus is looked at.
///
/// It costs two calls per input, which is nothing beside what the reader
/// itself does.
fn control() {
    let mut reply = [0u8; 24 + 8];
    reply[..4].copy_from_slice(&RESP_OK_CAPSET.to_le_bytes());
    let payload = virtgpu_abi::resp_capset_payload(&reply)
        .expect("a well-formed cap set reply must read as one");
    assert_eq!(
        payload.len(),
        8,
        "the payload must be what follows the header"
    );

    let mut declined = [0u8; 24 + 8];
    declined[..4].copy_from_slice(&RESP_ERR_INVALID_PARAMETER.to_le_bytes());
    assert!(
        virtgpu_abi::resp_capset_payload(&declined).is_none(),
        "an error reply must not be handed over as a cap set"
    );
}

fuzz_target!(|data: &[u8]| {
    control();

    match virtgpu_abi::resp_capset_payload(data) {
        Some(payload) => {
            // The invariant that matters: the payload is a sub-slice of the
            // bytes it came from and is never longer. A reader that computed a
            // length from the reply rather than bounding it by the buffer is
            // how a device talks a kernel into handing a caller memory nobody
            // wrote.
            assert!(
                payload.len() <= data.len(),
                "the payload is longer than the reply it was read from"
            );
            // It begins immediately after the header, so a caller reading
            // fields out of it reads the device's bytes rather than the
            // header's.
            assert_eq!(payload.as_ptr(), data[24..].as_ptr());
            // And it only comes back from a reply that really is a cap set. An
            // error answer read as a payload would be a capability word of
            // zeroes a caller believes.
            let header = virtgpu_abi::read_struct::<CtrlHeader>(data)
                .expect("a payload means there was a header to read");
            assert!(
                header.is(RESP_OK_CAPSET),
                "a payload came back from a reply that is not a cap set"
            );
        }
        None => {
            // `None` has exactly two causes, and a third would be this reader
            // inventing an outcome the driver cannot act on: the buffer is
            // shorter than a header, or the header names something else.
            let short = data.len() < size_of::<CtrlHeader>();
            let wrong_code = !short
                && !virtgpu_abi::read_struct::<CtrlHeader>(data)
                    .expect("long enough, checked above")
                    .is(RESP_OK_CAPSET);
            assert!(
                short || wrong_code,
                "a reply that is a cap set was refused: {} bytes",
                data.len()
            );
        }
    }
});
