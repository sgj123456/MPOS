//! Turn virtio-input key events into console bytes.
//!
//! The device reports evdev key codes with press/release/repeat values; the
//! line discipline wants the bytes a keyboard would have sent down a wire. This
//! maps the US layout, the control characters (so Ctrl-C is `0x03`),
//! Enter/Tab/Backspace/Escape, and the navigation keys to the `ESC [`-style
//! sequences the discipline's `csi_key` already decodes - so an arrow recalls
//! history and Delete erases the way they do over serial.
//!
//! Pure and host-tested: no device, no scheduler. The caller holds one
//! [`Keymap`] and feeds it every `EV_KEY` event in order, so it can track the
//! latched modifiers.

/// Latched modifier state, kept across events by the caller.
#[derive(Default)]
pub struct Keymap {
    shift: bool,
    ctrl: bool,
    caps: bool,
}

/// evdev `KEY_*` codes, only as far as the console uses them.
mod key {
    pub const ESC: u16 = 1;
    pub const ONE: u16 = 2; // KEY_1; KEY_9 = 10, KEY_0 = 11
    pub const MINUS: u16 = 12;
    pub const EQUAL: u16 = 13;
    pub const BACKSPACE: u16 = 14;
    pub const TAB: u16 = 15;
    pub const Q: u16 = 16; // .. P = 25
    pub const LEFTBRACE: u16 = 26;
    pub const RIGHTBRACE: u16 = 27;
    pub const ENTER: u16 = 28;
    pub const CTRL: u16 = 29;
    pub const A: u16 = 30; // .. L = 38
    pub const SEMICOLON: u16 = 39;
    pub const APOSTROPHE: u16 = 40;
    pub const GRAVE: u16 = 41;
    pub const SHIFT: u16 = 42;
    pub const BACKSLASH: u16 = 43;
    pub const Z: u16 = 44; // .. M = 50
    pub const COMMA: u16 = 51;
    pub const PERIOD: u16 = 52;
    pub const SLASH: u16 = 53;
    pub const SHIFT_R: u16 = 54;
    pub const SPACE: u16 = 57;
    pub const CAPSLOCK: u16 = 58;
    pub const HOME: u16 = 102;
    pub const UP: u16 = 103;
    pub const PAGEUP: u16 = 104;
    pub const LEFT: u16 = 105;
    pub const RIGHT: u16 = 106;
    pub const END: u16 = 107;
    pub const DOWN: u16 = 108;
    pub const PAGEDOWN: u16 = 109;
    pub const CTRL_R: u16 = 97;
    pub const DELETE: u16 = 211;
}

const ARROW_UP: &[u8] = b"\x1b[A";
const ARROW_DOWN: &[u8] = b"\x1b[B";
const ARROW_RIGHT: &[u8] = b"\x1b[C";
const ARROW_LEFT: &[u8] = b"\x1b[D";
const SEQ_HOME: &[u8] = b"\x1b[H";
const SEQ_END: &[u8] = b"\x1b[F";
const SEQ_DELETE: &[u8] = b"\x1b[3~";
const SEQ_PAGEUP: &[u8] = b"\x1b[5~";
const SEQ_PAGEDOWN: &[u8] = b"\x1b[6~";
const ENTER_BYTE: &[u8] = b"\r";
const TAB_BYTE: &[u8] = b"\t";
const BACKSPACE_BYTE: &[u8] = b"\x7f";
const ESC_BYTE: &[u8] = b"\x1b";

/// A key's answer: up to four bytes (most are one; arrows are three).
pub type Bytes = [u8; 4];

impl Keymap {
    pub const fn new() -> Keymap {
        Keymap {
            shift: false,
            ctrl: false,
            caps: false,
        }
    }

    /// Feed one `EV_KEY` event; return the bytes it produced, if any.
    ///
    /// `value` is evdev's: 0 release, 1 press, 2 auto-repeat. Modifiers latch on
    /// every edge and answer nothing; a key produces its byte(s) only on a press
    /// or a repeat.
    pub fn event(&mut self, code: u16, value: i32) -> Option<(Bytes, usize)> {
        // Modifier latching, both edges.
        match code {
            key::SHIFT | key::SHIFT_R => {
                self.shift = value != 0;
                return None;
            }
            key::CTRL | key::CTRL_R => {
                self.ctrl = value != 0;
                return None;
            }
            key::CAPSLOCK => {
                if value == 1 {
                    self.caps = !self.caps;
                }
                return None;
            }
            _ => {}
        }
        if value == 0 {
            return None; // a plain-key release is silent
        }

        // Fixed sequences (navigation, and the lone control keys).
        if let Some(seq) = self.sequence(code) {
            let mut out: Bytes = [0; 4];
            out[..seq.len()].copy_from_slice(seq);
            return Some((out, seq.len()));
        }

        // A computed single byte (letters, digits, punctuation).
        let byte = self.printable(code)?;
        let mut out: Bytes = [0; 4];
        out[0] = byte;
        Some((out, 1))
    }

    fn sequence(&self, code: u16) -> Option<&'static [u8]> {
        Some(match code {
            key::UP => ARROW_UP,
            key::DOWN => ARROW_DOWN,
            key::RIGHT => ARROW_RIGHT,
            key::LEFT => ARROW_LEFT,
            key::HOME => SEQ_HOME,
            key::END => SEQ_END,
            key::DELETE => SEQ_DELETE,
            key::PAGEUP => SEQ_PAGEUP,
            key::PAGEDOWN => SEQ_PAGEDOWN,
            key::ENTER => ENTER_BYTE,
            key::TAB => TAB_BYTE,
            key::BACKSPACE => BACKSPACE_BYTE,
            key::ESC => ESC_BYTE,
            _ => return None,
        })
    }

    /// The single byte a printable key stands for, honouring shift and control.
    fn printable(&mut self, code: u16) -> Option<u8> {
        // Letters. The row blocks are 16..=25 (Q-P), 30..=38 (A-L), 44..=50 (Z-M).
        let letter = match code {
            key::Q..=25 => b"qwertyuiop"[(code - key::Q) as usize],
            key::A..=38 => b"asdfghjkl"[(code - key::A) as usize],
            key::Z..=50 => b"zxcvbnm"[(code - key::Z) as usize],
            _ => 0,
        };
        if letter != 0 {
            let upper = self.shift ^ self.caps;
            let c = if upper {
                letter.to_ascii_uppercase()
            } else {
                letter
            };
            return Some(if self.ctrl { c & 0x1f } else { c });
        }
        // Digits 1..9,0.
        if (key::ONE..=11).contains(&code) {
            let i = (code - key::ONE) as usize;
            let base = b"1234567890"[i];
            let shifted = b"!@#$%^&*()"[i];
            return Some(if self.shift { shifted } else { base });
        }
        let (plain, shifted) = match code {
            key::SPACE => (b' ', b' '),
            key::MINUS => (b'-', b'_'),
            key::EQUAL => (b'=', b'+'),
            key::LEFTBRACE => (b'[', b'{'),
            key::RIGHTBRACE => (b']', b'}'),
            key::SEMICOLON => (b';', b':'),
            key::APOSTROPHE => (b'\'', b'"'),
            key::GRAVE => (b'`', b'~'),
            key::BACKSLASH => (b'\\', b'|'),
            key::COMMA => (b',', b'<'),
            key::PERIOD => (b'.', b'>'),
            key::SLASH => (b'/', b'?'),
            _ => return None,
        };
        Some(if self.shift { shifted } else { plain })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_follow_shift_and_caps() {
        let mut k = Keymap::new();
        let (b, n) = k.event(key::A, 1).unwrap();
        assert_eq!(&b[..n], b"a");
        k.event(key::SHIFT, 1); // hold shift
        let (b, n) = k.event(key::A, 1).unwrap();
        assert_eq!(&b[..n], b"A");
    }

    #[test]
    fn ctrl_c_is_the_interrupt_byte() {
        let mut k = Keymap::new();
        k.event(key::CTRL, 1);
        let (b, n) = k.event(46, 1).unwrap(); // KEY_C
        assert_eq!(&b[..n], &[0x03]);
    }

    #[test]
    fn enter_tab_backspace_are_control_bytes() {
        let mut k = Keymap::new();
        let (b, n) = k.event(key::ENTER, 1).unwrap();
        assert_eq!(&b[..n], b"\r");
        let (b, n) = k.event(key::BACKSPACE, 1).unwrap();
        assert_eq!(&b[..n], b"\x7f");
    }

    #[test]
    fn arrows_and_delete_use_the_csi_sequences() {
        let mut k = Keymap::new();
        let (b, n) = k.event(key::UP, 1).unwrap();
        assert_eq!(&b[..n], b"\x1b[A");
        let (b, n) = k.event(key::DELETE, 1).unwrap();
        assert_eq!(&b[..n], b"\x1b[3~");
    }

    #[test]
    fn a_release_is_silent_but_still_unlatches() {
        let mut k = Keymap::new();
        k.event(key::SHIFT, 1);
        assert!(k.event(key::A, 0).is_none()); // release of 'a': nothing
        k.event(key::SHIFT, 0); // release shift
        let (b, n) = k.event(key::A, 1).unwrap();
        assert_eq!(&b[..n], b"a"); // lowercase again
    }

    #[test]
    fn shifted_digits_become_symbols() {
        let mut k = Keymap::new();
        k.event(key::SHIFT, 1);
        let (b, n) = k.event(key::ONE, 1).unwrap(); // KEY_1
        assert_eq!(&b[..n], b"!");
    }
}
