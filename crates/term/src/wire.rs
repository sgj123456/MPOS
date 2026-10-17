//! Where a byte stream may be cut on its way to a terminal that is not this
//! one.
//!
//! The console has two sinks, and only one of them can refuse bytes: the wire
//! a host terminal sits at the far end of, when the wire is a console but not
//! the preferred one (`console=ttyS0 console=tty0`). That copy is best-effort
//! on purpose - a host terminal that cannot keep up must not throttle the
//! screen somebody is reading - so a full transmit ring drops the tail of a
//! write.
//!
//! *Where* it drops is the whole of this module. Cut in the middle of an
//! escape sequence and the emulator at the far end is left inside one: it
//! swallows the bytes that follow - which are the next screenful - until some
//! later byte happens to end a sequence, and the state changes that arrived in
//! between (`ESC [ ? 1049h`, `ESC [ ? 25l`) are read as parameters of a
//! command nobody sent. A `vi` page-turn is a few kilobytes of sequences, so
//! one cut mid-sequence costs more than the bytes it dropped: the screen stops
//! making sense, the cursor stays hidden after the program exits, and the
//! terminal is left on an alternate screen the shell is not on.
//!
//! Cut *between* sequences and the loss is only content: the emulator is in
//! its ground state, it understands everything it was given, and what it did
//! not get is a repaint it never saw. A whole screenful is still coherent.
//!
//! So [`cut`] answers the length of the longest prefix that ends in the ground
//! state. It is a scanner over the same states the emulator parses with, and it
//! is deliberately stateless across calls: a write that is cut is followed by
//! bytes the terminal never sees the head of, and guessing where that tail
//! resumed would be a second parser with a second opinion. Losing the rest of
//! the chunk is the honest answer, and it is the one this module can give
//! without being told what came before.

/// Which of the states a terminal reading the wire can be in.
///
/// Mirrors [`super::State`] in the grid, and for the same reason the two
/// parsers exist apart: this one is asked only "is it between sequences", and
/// answerable without owning a screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    /// Between sequences: ordinary bytes, and the end of every sequence.
    Ground,
    /// An `ESC` has been read and the next byte says which one.
    Escape,
    /// Inside a `CSI` sequence - parameters, intermediates and its final byte.
    Csi,
    /// Inside a character-set choice - `ESC ( B` - whose third byte is the
    /// designator. Two bytes of it are not a sequence anybody has finished.
    Charset,
    /// Inside an `OSC` string, which ends on `BEL` or on `ST`.
    Osc,
    /// Inside a `DCS`/`SOS` string, which ends on `ST` alone. Kept apart from
    /// [`Stream::Osc`] because a `BEL` inside one is data, and cutting there
    /// would leave the far end reading a string that goes on without it.
    Dcs,
}

impl Stream {
    /// One byte of the wire, and the state it leaves the reader in.
    ///
    /// The byte classes are the ones the grid's own parser uses, because a cut
    /// the grid would not have made is a cut that desynchronises it: `ESC`
    /// opens, `ESC [` is a `CSI`, `ESC ]` and `ESC P`/`ESC X` open strings, and
    /// an `ESC` followed by anything else is one of the two-byte escapes
    /// (`ESC D`, `ESC 7`, `ESC ( B`) and is over at once.
    pub fn step(self, byte: u8) -> Stream {
        match (self, byte) {
            // A second `ESC` restarts rather than nesting: `ESC ESC [ A` is a
            // discarded `ESC` and then a complete cursor-up.
            (_, 0x1b) => Stream::Escape,
            // `CAN` and `SUB` cancel a sequence wherever they land, which the
            // grid honours the same way - and a cut after one is a cut between
            // sequences, not inside the one it abandoned.
            (_, 0x18 | 0x1a) => Stream::Ground,
            (Stream::Escape, b'[') => Stream::Csi,
            (Stream::Escape, b']') => Stream::Osc,
            (Stream::Escape, b'P' | b'X') => Stream::Dcs,
            // `ESC (` and `ESC )` name a charset in the byte that follows.
            (Stream::Escape, 0x20..=0x2f) => Stream::Charset,
            (Stream::Charset, _) => Stream::Ground,
            (Stream::Escape, _) => Stream::Ground,
            // A `CSI` runs to its final byte, which is anything from `@` to
            // `~`; the parameters and intermediates are read by the grid, not
            // here, because none of them can end the sequence.
            (Stream::Csi, 0x20..=0x3f) => Stream::Csi,
            (Stream::Csi, _) => Stream::Ground,
            (Stream::Osc, 0x07) => Stream::Ground,
            (Stream::Osc | Stream::Dcs, _) => self,
            _ => Stream::Ground,
        }
    }
}

/// How many bytes of `bytes` may go on the wire on their own: the length of
/// the longest prefix of at most `limit` bytes that leaves a terminal between
/// sequences.
///
/// `0` means not even the first sequence fits, which is a cut before it starts
/// - and the whole chunk is then better off unsent than sent half of.
///
/// ```
/// use mpos_term::wire::cut;
///
/// // Plain text can be cut anywhere.
/// assert_eq!(cut(b"hello", 3), 3);
/// // A cut that would land inside a CSI stops *before* its ESC: sending the
/// // parameters alone would leave the far end interpreting half a sequence.
/// let stream = b"abc\x1b[1;24rdef";
/// assert_eq!(cut(stream, 5), 3); // one byte into the CSI
/// assert_eq!(cut(stream, 10), 10); // the terminator completes it
/// // A limit shorter than the first sequence cuts nothing.
/// assert_eq!(cut(b"\x1b[1;24r", 3), 0);
/// ```
pub fn cut(bytes: &[u8], limit: usize) -> usize {
    let mut state = Stream::Ground;
    let mut safe = 0;
    for (i, &b) in bytes.iter().enumerate().take(limit) {
        state = state.step(b);
        if state == Stream::Ground {
            safe = i + 1;
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream that is nothing but text can be cut anywhere.
    #[test]
    fn plain_text_cuts_at_the_limit() {
        assert_eq!(cut(b"hello world", 5), 5);
        assert_eq!(cut(b"hello world", 11), 11);
        assert_eq!(cut(b"hello world", 0), 0);
    }

    /// The measured reason this exists: a cut that lands inside the `CSI` of a
    /// page-turn has to stop before the `ESC`, not in the middle of what
    /// follows it.
    #[test]
    fn a_cut_inside_a_sequence_stops_at_its_start() {
        // `ESC [ 1;24r` is the seven bytes 3..=9, `ESC [ 12M` the four 10..=14.
        let stream = b"abc\x1b[1;24r\x1b[12Mdef";
        assert_eq!(cut(stream, 3), 3, "the text before the first sequence");
        assert_eq!(cut(stream, 5), 3, "one byte into the CSI: cut before it");
        assert_eq!(cut(stream, 9), 3, "still inside it, one byte short");
        assert_eq!(cut(stream, 10), 10, "the terminator completes it");
        assert_eq!(cut(stream, 14), 10, "and the next one is not half sent");
        assert_eq!(cut(stream, 15), 15, "its terminator completes that too");
    }

    /// The two-byte escapes are over as soon as their second byte arrives -
    /// `ESC D` and `ESC M` are how a screen scrolls, and a cut between them
    /// would leave an `ESC` the far end is still waiting to interpret.
    #[test]
    fn a_two_byte_escape_is_never_cut_in_half() {
        assert_eq!(cut(b"ab\x1bDcd", 2), 2);
        assert_eq!(cut(b"ab\x1bDcd", 3), 2);
        assert_eq!(cut(b"ab\x1bDcd", 4), 4);
    }

    /// A character-set choice is three bytes, not two: `ESC ( B`.
    #[test]
    fn a_charset_choice_is_never_cut_in_half() {
        assert_eq!(cut(b"\x1b(Bx", 1), 0);
        assert_eq!(cut(b"\x1b(Bx", 2), 0);
        assert_eq!(cut(b"\x1b(Bx", 3), 3);
    }

    /// Strings run to their terminator. An `OSC` the far end is still reading
    /// would eat every byte after it, which is worse than the screenful it
    /// was going to lose.
    #[test]
    fn a_string_runs_to_its_terminator() {
        let osc = b"\x1b]0;title\x07after";
        assert_eq!(cut(osc, 4), 0, "inside the string: nothing is safe");
        assert_eq!(cut(osc, 11), 11, "the BEL ends it");
        // `ST` (`ESC \`) ends one just the same.
        let st = b"\x1b]0;t\x1b\\after";
        assert_eq!(cut(st, 6), 0);
        assert_eq!(cut(st, 7), 7);
    }

    /// A second `ESC` restarts rather than nesting: `ESC ESC [ A` is an `ESC`
    /// the terminal discards followed by a complete cursor-up.
    #[test]
    fn a_second_escape_restarts_the_sequence() {
        assert_eq!(cut(b"\x1b\x1b[A", 1), 0);
        assert_eq!(cut(b"\x1b\x1b[A", 2), 0);
        assert_eq!(cut(b"\x1b\x1b[A", 4), 4);
    }
}
