//! t-getrandom - one test for getrandom, which a trace of a musl program shows
//! twice at startup (16 bytes and 8 bytes, `GRND_NONBLOCK`): Rust std seeds its
//! hasher with it.
//!
//! Pinned here: a blocking fill of n bytes answers exactly n, the
//! `GRND_NONBLOCK` variant answers n too (this kernel always has entropy to
//! spare), and a zero-length request answers 0 without touching the buffer.
//! Randomness itself cannot be asserted - only the counts, and that a large fill
//! answers its exact length rather than short-reading.

use mpos_user_tests::Report;
use rustix::rand::{GetRandomFlags, getrandom};

fn main() {
    println!("t-getrandom");
    let mut r = Report::new();

    let mut a = [0u8; 16];
    r.check(
        "a 16-byte fill answers 16",
        getrandom(&mut a[..], GetRandomFlags::empty()) == Ok(16),
    );

    let mut b = [0u8; 16];
    r.check(
        "the non-blocking fill answers 16 too",
        getrandom(&mut b[..], GetRandomFlags::NONBLOCK) == Ok(16),
    );

    let mut c = [0u8; 1];
    r.check(
        "a 1-byte fill answers 1",
        getrandom(&mut c[..], GetRandomFlags::empty()) == Ok(1),
    );

    // The type is spelled on the binding: an empty literal slice is also a
    // `MaybeUninit` slice as far as inference is concerned, and `getrandom` takes
    // both.
    let empty: &mut [u8] = &mut [];
    r.check(
        "a zero-length request answers 0",
        getrandom(empty, GetRandomFlags::empty()) == Ok(0),
    );

    // A whole page of entropy in one call: the exact count comes back, so a
    // request that would have to be split cannot answer short without saying so.
    let mut big = [0u8; 4096];
    r.check(
        "a 4096-byte fill answers 4096",
        getrandom(&mut big[..], GetRandomFlags::empty()) == Ok(4096),
    );

    // Different sizes, same story: never an error.
    let mut d = [0u8; 7];
    r.check(
        "a 7-byte fill succeeds",
        getrandom(&mut d[..], GetRandomFlags::empty()).is_ok(),
    );
    let mut e = [0u8; 100];
    r.check(
        "a 100-byte fill succeeds",
        getrandom(&mut e[..], GetRandomFlags::empty()).is_ok(),
    );

    r.finish()
}
