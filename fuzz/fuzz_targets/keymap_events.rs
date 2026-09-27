#![no_main]

//! evdev `EV_KEY` events turned into console bytes.
//!
//! `Keymap::event` is fed scancode numbers straight off a virtio-input device,
//! and a hot-plugged or emulated device can claim any `u16`. It holds latched
//! modifier state, so the sequence matters as much as each event: the failure
//! this looks for is a code path that reads the latch it forgot to set, or that
//! returns a length past the four bytes it was given.

use arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use mpos::vfs::term::keymap::Keymap;

#[derive(Arbitrary, Debug)]
struct Input {
    /// `(code, value)` pairs, in the order a device would have sent them.
    events: Vec<(u16, i32)>,
}

fuzz_target!(|input: Input| {
    let mut keymap = Keymap::default();
    let mut produced = 0usize;

    for &(code, value) in input.events.iter().take(512) {
        if let Some((bytes, len)) = keymap.event(code, value) {
            assert!(
                len <= bytes.len(),
                "key code {code} reported {len} bytes out of {}",
                bytes.len()
            );
            assert!(len > 0, "key code {code} produced an empty keypress");
            // Nothing a keymap can emit is a NUL: it would end the line the
            // tty layer is building.
            assert!(
                bytes[..len].iter().all(|&b| b != 0),
                "key code {code} value {value} emitted a NUL: {bytes:?}"
            );
            produced += len;
        }
    }

    // A release of a key nobody pressed and a press of a code past the table
    // are both ordinary; what neither may do is produce output for a key that
    // is only a modifier.
    let mut modifiers_only = Keymap::default();
    for code in [29u16, 97, 42, 54, 58, 64] {
        for value in [-1i32, 0, 1, 2, 3] {
            assert!(
                modifiers_only.event(code, value).is_none(),
                "modifier code {code} value {value} produced bytes"
            );
        }
    }
    assert!(produced <= input.events.len() * 4);
});
