//! The tty: a line discipline between the machine's input and its readers.
//!
//! Bytes arrive one at a time - today from whoever calls [`Tty::receive_byte`],
//! tomorrow from a serial interrupt - and what happens to them depends on the
//! settings in [`Termios`]: in canonical mode they are edited into a line here
//! and a read takes a whole line; in raw mode they go straight to the reader,
//! which is how a shell does its own editing and how arrow keys reach it as
//! the sequences a terminal actually sent.
//!
//! Editing is more than backspace. The cursor moves through the line, arrows
//! walk the history, and keys arrive as escape sequences - `ESC [ D` for left,
//! `ESC [ 3 ~` for delete, `ESC O A` for up - which have to be recognised as
//! one key rather than as four typed characters. Sequences with no editing
//! meaning, including a terminal's answers to what a shell asked it (`ESC[6n`
//! gets `ESC[41;5R` back), are dropped rather than typed: that is the
//! difference between a working arrow key and a prompt full of `[41;5R`.
//!
//! [`LineDiscipline`] is the pure state machine and runs under `cargo test` on
//! the host. [`Tty`] owns one under a mutex, echoes to the console as bytes are
//! received, and turns `read` into a blocking read via
//! [`crate::sched::block_until`].
//!
//! The standard descriptors fd 0, 1 and 2 are three open descriptions of this
//! one inode, created at start-up (`vfs::file::init_standard`). A `/dev/tty`
//! node waits until fds are per-process (M12), when a shell can open one;
//! until then nothing user side names this device.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::termios::{
    ECHO, ECHOCTL, ICANON, ICRNL, IEXTEN, IGNCR, INLCR, ISIG, ISTRIP, IXON, Termios, VEOF, VERASE,
    VINTR, VKILL, VLNEXT, VMIN, VQUIT, VREPRINT, VSTART, VSTOP, VSUSP, VWERASE,
};
use super::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
use crate::sched::policy::TaskId;
#[cfg(target_os = "none")]
use crate::sched::signal::SIGINT;

const LF: u8 = b'\n';
const CR: u8 = b'\r';
/// The other key terminals send for erase. `VERASE` is `0x7f`, but a terminal
/// in raw mode may send `^H` instead, and both mean the same thing to whoever
/// is typing.
const BACKSPACE: u8 = 0x08;

/// How many entered lines the up arrow can walk back through.
const HISTORY_MAX: usize = 64;
/// How many bytes of an escape sequence's parameters are gathered before the
/// sequence is given up as a mistake.
const CSI_MAX: usize = 16;
/// Cap on the body of an OSC string tracked on the output, so a colour query
/// can be recognised; a longer run is a colour *set*, which is just forwarded.
const OSC_MAX: usize = 32;

/// Recognise a colour *query* OSC body - `10;?` (foreground) or `11;?`
/// (background), which `set_color`/a theme probe send - and name the reply. A
/// body that is not a query (a colour *set* like `11;rgb:...`) returns `None`.
fn osc_color_query(body: &[u8]) -> Option<(usize, &'static str)> {
    if body.starts_with(b"10;?") {
        Some((10, "rgb:dcdcdc/dcdcdc/dcdcdc"))
    } else if body.starts_with(b"11;?") {
        Some((11, "rgb:1c1c/1c1c/1c1c"))
    } else {
        None
    }
}
/// How much of the current output line is remembered for redrawing. A prompt
/// fits comfortably; a program that prints a screenful without a newline does
/// not need to be replayed by the editor.
const PROMPT_MAX: usize = 128;

/// What one input byte asked for, beyond being edited into the line.
///
/// The discipline is pure: it decides *that* the interrupt was asked for, and
/// leaves the sending to [`Tty`], which is the only thing that knows who to
/// send it to.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// The foreground reader is to be interrupted.
    Interrupt,
}

/// Where an escape sequence has got to. Anything other than [`Ground`](Self::Ground)
/// means bytes are being gathered into a key, not typed into a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Esc {
    Ground,
    /// Saw `ESC`.
    Start,
    /// Saw `ESC [`: control sequence, parameters being gathered.
    Csi,
    /// Saw `ESC O`: single-shift, one byte left to read.
    Ss3,
    /// Saw `ESC ]`: an operating-system command, ended by BEL.
    Osc,
    /// A sequence that swallows exactly one more byte - a character-set
    /// choice, or a string control's terminator.
    One,
}

/// A key that is not a character: what the arrow, editing and keypad keys
/// send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    PageUp,
    PageDown,
}

/// Where the cursor is on the screen, as far as this kernel can tell.
///
/// A shell asks the terminal where the cursor is (`ESC[6n`) before it draws
/// anything, and waits for the answer before it reads another key. Nobody
/// answers a question asked into a serial port unless the line discipline
/// does - so it keeps enough of a display in mind to answer, and lets the
/// host terminal do the drawing.
#[derive(Clone, Copy, Debug)]
struct Screen {
    row: usize,
    col: usize,
    rows: usize,
    cols: usize,
}

impl Screen {
    const fn new() -> Screen {
        Screen {
            row: 0,
            col: 0,
            rows: 25,
            cols: 80,
        }
    }

    fn put(&mut self) {
        self.col += 1;
        if self.col >= self.cols {
            self.col = 0;
            self.newline();
        }
    }

    fn newline(&mut self) {
        if self.row + 1 < self.rows {
            self.row += 1;
        }
        self.col = 0;
    }

    fn left(&mut self, n: usize) {
        self.col = self.col.saturating_sub(n);
    }

    fn right(&mut self, n: usize) {
        self.col = (self.col + n).min(self.cols - 1);
    }

    fn up(&mut self, n: usize) {
        self.row = self.row.saturating_sub(n);
    }

    fn down(&mut self, n: usize) {
        self.row = (self.row + n).min(self.rows - 1);
    }

    /// Tab stops every eight columns, which is where every terminal puts them.
    fn tab(&mut self) {
        self.col = ((self.col / 8) + 1) * 8;
        if self.col >= self.cols {
            self.col = self.cols - 1;
        }
    }
}

/// The editing state machine: input bytes in through [`receive`](Self::receive),
/// completed lines and console echo out.
pub struct LineDiscipline {
    /// Bytes waiting for a reader. Canonical mode only ever queues whole
    /// lines; a reader is free to take less than one.
    out: VecDeque<u8>,
    /// The terminal's own answers, kept apart from typed input.
    ///
    /// A shell asks where the cursor is and then reads - greedily, taking
    /// everything that is there. An answer queued behind it would come back
    /// in the same read as the beginning of the next command, and a reader
    /// that reaches past the answer for the keys after it loses them. So an
    /// answer travels on its own, and is read on its own.
    answer: VecDeque<u8>,
    /// A read taken over this reports 0: the end of input, which is not a
    /// character and never becomes one.
    eof: bool,
    /// The line being typed, and where the cursor stands within it.
    pending: Vec<u8>,
    cursor: usize,
    /// What the console should show, since the last drain.
    echo: Vec<u8>,
    /// The settings that decide everything above.
    tio: Termios,
    /// Where an escape sequence has got to.
    esc: Esc,
    /// The parameter bytes of a `CSI` sequence in flight: `ESC [ 1 ; 5 A`.
    csi: Vec<u8>,
    /// Taken literally rather than as a control character: set by `^V`.
    literal: bool,
    /// Lines already entered, newest last, for the up and down arrows.
    history: Vec<Vec<u8>>,
    /// Which history line is on screen, if one is.
    hist_at: Option<usize>,
    /// The line being typed when the up arrow was first pressed, so coming
    /// back down past the newest history returns it.
    stash: Vec<u8>,
    /// What has been printed on the current output line - the shell's prompt,
    /// in practice. Redrawing the input line means printing it again, and a
    /// discipline that only sees its own echo has no other way to know it.
    prompt: Vec<u8>,
    /// Where an escape sequence in the *output* has got to, so the prompt
    /// recorder swallows the whole sequence instead of storing a stray `[`.
    out_esc: Esc,
    /// The parameters of an output sequence in flight, for the ones whose
    /// numbers mean something - a cursor move, or a question.
    out_params: Vec<u8>,
    /// The body of an OSC string in flight on the *output*, so a colour
    /// query (`OSC 10;?`/`OSC 11;?`) can be recognised and answered.
    out_osc: Vec<u8>,
    /// A DCS string (`ESC P ...`, e.g. XTGETTCap) is in flight rather than a
    /// plain OSC - DCS ends only on `ESC \`, never on BEL.
    out_dcs: bool,
    /// Whether the sequence currently in flight is an OSC or DCS string (as
    /// opposed to a charset selection that also passes through `Esc::One`), so
    /// a terminating `ESC \` knows to finish it.
    in_string_seq: bool,
    /// Where the cursor stands, to answer a shell that asks.
    screen: Screen,
}

impl Default for LineDiscipline {
    fn default() -> LineDiscipline {
        LineDiscipline::new()
    }
}

impl LineDiscipline {
    pub const fn new() -> LineDiscipline {
        LineDiscipline {
            out: VecDeque::new(),
            answer: VecDeque::new(),
            eof: false,
            pending: Vec::new(),
            cursor: 0,
            echo: Vec::new(),
            tio: Termios::console_default(),
            esc: Esc::Ground,
            csi: Vec::new(),
            literal: false,
            history: Vec::new(),
            hist_at: None,
            stash: Vec::new(),
            prompt: Vec::new(),
            out_esc: Esc::Ground,
            out_params: Vec::new(),
            out_osc: Vec::new(),
            out_dcs: false,
            in_string_seq: false,
            screen: Screen::new(),
        }
    }

    /// Feed one input byte, queueing whatever the console should echo, and
    /// report anything the byte asked for beyond editing.
    pub fn receive(&mut self, byte: u8) -> Action {
        // Mid-sequence, a byte belongs to the sequence. Raw mode never gets
        // here: it hands every byte over unexamined.
        if self.esc != Esc::Ground {
            self.esc_byte(byte);
            return Action::None;
        }
        if self.literal {
            self.literal = false;
            self.insert(byte);
            return Action::None;
        }

        // Signals first: an interrupt is not a character of any line, and it
        // reaches the reader whether the terminal is cooked or raw.
        if self.tio.lflag & ISIG != 0 {
            if byte == self.tio.cc[VINTR] {
                self.echo_control(b'^', b'C');
                return Action::Interrupt;
            }
            // No SIGQUIT and no job control yet: the keys are recognised so
            // they do not become input, and then have nothing to do.
            if byte == self.tio.cc[VQUIT] || byte == self.tio.cc[VSUSP] {
                return Action::None;
            }
        }
        // Flow control is between the terminal and the line, not part of it.
        if self.tio.iflag & IXON != 0 && (byte == self.tio.cc[VSTOP] || byte == self.tio.cc[VSTART])
        {
            return Action::None;
        }

        let mut byte = byte;
        if self.tio.iflag & ISTRIP != 0 {
            byte &= 0x7f;
        }
        if byte == CR {
            if self.tio.iflag & IGNCR != 0 {
                return Action::None;
            }
            if self.tio.iflag & ICRNL != 0 {
                byte = LF;
            }
        } else if byte == LF && self.tio.iflag & INLCR != 0 {
            byte = CR;
        }

        if self.tio.lflag & ICANON == 0 {
            self.raw(byte);
            return Action::None;
        }
        self.canonical(byte)
    }

    /// Raw input: the byte is the reader's problem, editing included.
    fn raw(&mut self, byte: u8) {
        self.out.push_back(byte);
        if self.echo_on() {
            self.echo.push(byte);
        }
    }

    /// One byte of a line being typed.
    fn canonical(&mut self, byte: u8) -> Action {
        let eof = self.tio.cc[VEOF];
        let erase = self.tio.cc[VERASE];
        let kill = self.tio.cc[VKILL];
        let werase = self.tio.cc[VWERASE];
        let reprint = self.tio.cc[VREPRINT];
        let lnext = self.tio.cc[VLNEXT];
        match byte {
            b'\x1b' if self.tio.lflag & IEXTEN != 0 => {
                self.esc = Esc::Start;
                Action::None
            }
            LF => {
                self.complete_line();
                Action::None
            }
            // Without `ICRNL` a CR is an ordinary character; with it, this is
            // never reached, because the byte was turned into a LF above.
            CR => {
                self.insert(CR);
                Action::None
            }
            // Tab, which a shell takes as completion and a line takes as
            // itself: it is not an editing key, so it belongs in the line.
            0x09 => {
                self.insert(0x09);
                Action::None
            }
            b if b == eof => {
                // Mid-line: flush without the newline. Empty: end of input.
                if self.pending.is_empty() {
                    self.eof = true;
                } else {
                    self.flush();
                }
                Action::None
            }
            b if b == erase || b == BACKSPACE => {
                self.backspace();
                Action::None
            }
            b if b == kill => {
                self.kill_line();
                Action::None
            }
            b if b == werase => {
                self.erase_word();
                Action::None
            }
            b if b == reprint => {
                self.echo.push(CR);
                self.redraw();
                Action::None
            }
            b if b == lnext => {
                self.literal = true;
                Action::None
            }
            // The keys a real line editor offers. A shell that runs in raw
            // mode brings its own; these are for the one this kernel edits.
            // The control characters termios can name are matched above, so
            // these are the ones it cannot: a program that moves ^D elsewhere
            // gets 0x04 as an ordinary byte rather than a surprise.
            0x01 => {
                self.go_home();
                Action::None
            } // ^A
            0x02 => self.go_left(), // ^B
            0x05 => {
                self.go_end();
                Action::None
            } // ^E
            0x06 => self.go_right(), // ^F
            0x0b => {
                self.kill_to_end();
                Action::None
            } // ^K
            0x0c => self.clear_screen(), // ^L
            0x0e => self.history_next(), // ^N
            0x10 => self.history_prev(), // ^P
            0x14 => {
                self.transpose();
                Action::None
            } // ^T
            // Printable, and the bytes of a UTF-8 character beyond ASCII.
            b if b >= 0x20 && b != 0x7f => {
                self.insert(b);
                Action::None
            }
            // Any other control byte has no editing meaning; keep it out of
            // the line rather than passing it through raw.
            _ => Action::None,
        }
    }

    // --- editing the line ---

    fn insert(&mut self, byte: u8) {
        if self.cursor >= self.pending.len() {
            self.pending.push(byte);
        } else {
            self.pending.insert(self.cursor, byte);
        }
        self.cursor += 1;
        if self.echo_on() {
            self.echo.push(byte);
            // Typed into the middle: everything after the cursor has to be
            // pushed along on screen, and the cursor put back where it was.
            if self.cursor < self.pending.len() {
                self.echo.extend_from_slice(&self.pending[self.cursor..]);
                self.echo_left(self.pending.len() - self.cursor);
            }
        }
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.pending.remove(self.cursor);
        if self.echo_on() {
            if self.cursor == self.pending.len() {
                // Erase on a terminal: step back, blank, step back again.
                self.echo.extend_from_slice(b"\x08 \x08");
            } else {
                self.redraw();
            }
        }
    }

    /// Delete under the cursor, which is what the Delete key does - the key
    /// `erase` is not, since that one deletes behind it.
    fn delete_at_cursor(&mut self) {
        if self.cursor >= self.pending.len() {
            return;
        }
        self.pending.remove(self.cursor);
        if self.echo_on() {
            // DCH: delete one character under the cursor, closing the gap.
            self.echo.extend_from_slice(b"\x1b[P");
        }
    }

    fn kill_line(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.pending.clear();
        self.cursor = 0;
        if self.echo_on() {
            self.redraw();
        }
    }

    fn kill_to_end(&mut self) {
        if self.cursor >= self.pending.len() {
            return;
        }
        self.pending.truncate(self.cursor);
        if self.echo_on() {
            self.echo.extend_from_slice(b"\x1b[K");
        }
    }

    /// `^W`: the word behind the cursor, and the spaces in front of it.
    fn erase_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut end = self.cursor;
        while end > 0 && self.pending[end - 1] == b' ' {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && self.pending[start - 1] != b' ' {
            start -= 1;
        }
        self.pending.drain(start..self.cursor);
        self.cursor = start;
        if self.echo_on() {
            self.redraw();
        }
    }

    /// `^T`: swap the two characters behind the cursor.
    fn transpose(&mut self) {
        if self.cursor < 2 || self.cursor > self.pending.len() {
            return;
        }
        self.pending.swap(self.cursor - 2, self.cursor - 1);
        if self.echo_on() {
            self.redraw();
        }
    }

    fn go_left(&mut self) -> Action {
        if self.cursor == 0 {
            return Action::None;
        }
        self.cursor -= 1;
        if self.echo_on() {
            self.echo.extend_from_slice(b"\x1b[D");
        }
        Action::None
    }

    fn go_right(&mut self) -> Action {
        if self.cursor >= self.pending.len() {
            return Action::None;
        }
        self.cursor += 1;
        if self.echo_on() {
            self.echo.extend_from_slice(b"\x1b[C");
        }
        Action::None
    }

    fn go_home(&mut self) -> Action {
        if self.cursor == 0 {
            return Action::None;
        }
        self.cursor = 0;
        if self.echo_on() {
            self.echo.push(CR);
            self.echo_right(self.prompt.len());
        }
        Action::None
    }

    fn go_end(&mut self) -> Action {
        if self.cursor >= self.pending.len() {
            return Action::None;
        }
        let n = self.pending.len() - self.cursor;
        self.cursor = self.pending.len();
        if self.echo_on() {
            self.echo_right(n);
        }
        Action::None
    }

    fn clear_screen(&mut self) -> Action {
        if self.echo_on() {
            self.echo.extend_from_slice(b"\x1b[2J\x1b[H");
            self.redraw();
        }
        Action::None
    }

    // --- history ---

    fn history_prev(&mut self) -> Action {
        if self.history.is_empty() {
            return Action::None;
        }
        if self.hist_at.is_none() {
            self.stash = self.pending.clone();
            self.hist_at = Some(self.history.len());
        }
        let at = self.hist_at.unwrap_or(self.history.len());
        if at == 0 {
            return Action::None;
        }
        self.hist_at = Some(at - 1);
        self.pending = self.history[at - 1].clone();
        self.cursor = self.pending.len();
        self.redraw();
        Action::None
    }

    fn history_next(&mut self) -> Action {
        let at = match self.hist_at {
            Some(at) => at,
            None => return Action::None,
        };
        if at + 1 < self.history.len() {
            self.hist_at = Some(at + 1);
            self.pending = self.history[at + 1].clone();
        } else {
            // Past the newest: back to what was being typed before.
            self.hist_at = None;
            self.pending = core::mem::take(&mut self.stash);
        }
        self.cursor = self.pending.len();
        self.redraw();
        Action::None
    }

    fn remember(&mut self, line: &[u8]) {
        if line.is_empty() || self.history.last().map(Vec::as_slice) == Some(line) {
            return;
        }
        self.history.push(line.to_vec());
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
    }

    // --- completing input ---

    fn complete_line(&mut self) {
        self.pending.push(LF);
        if self.echo_on() {
            self.echo.push(LF);
        }
        let line = core::mem::take(&mut self.pending);
        self.cursor = 0;
        self.hist_at = None;
        self.remember(&line[..line.len() - 1]);
        self.out.extend(line.iter().copied());
    }

    /// `^D` mid-line: hand over what is typed, without a newline.
    fn flush(&mut self) {
        if self.echo_on() {
            self.echo.push(LF);
        }
        let line = core::mem::take(&mut self.pending);
        self.cursor = 0;
        self.hist_at = None;
        self.remember(&line);
        self.out.extend(line);
    }

    // --- echo ---

    fn echo_on(&self) -> bool {
        self.tio.lflag & ECHO != 0
    }

    /// Show `^C` for the key that interrupted, the way a terminal does, so the
    /// line on screen still matches what was typed.
    fn echo_control(&mut self, first: u8, second: u8) {
        if self.echo_on() && self.tio.lflag & ECHOCTL != 0 {
            self.echo.push(first);
            self.echo.push(second);
            self.echo.push(LF);
        }
    }

    fn echo_left(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.echo.extend_from_slice(b"\x1b[");
        self.echo.extend_from_slice(Self::decimal(n).as_slice());
        self.echo.push(b'D');
    }

    fn echo_right(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.echo.extend_from_slice(b"\x1b[");
        self.echo.extend_from_slice(Self::decimal(n).as_slice());
        self.echo.push(b'C');
    }

    fn decimal(mut n: usize) -> Vec<u8> {
        let mut digits = [0u8; 8];
        let mut at = digits.len();
        if n == 0 {
            return vec![b'0'];
        }
        while n > 0 {
            at -= 1;
            digits[at] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        digits[at..].to_vec()
    }

    /// Put the line on screen again from the prompt onwards - after a history
    /// move, a word erased mid-line, or a `^L`.
    fn redraw(&mut self) {
        if !self.echo_on() {
            return;
        }
        self.echo.push(CR);
        self.echo.extend_from_slice(&self.prompt);
        // EL: erase to the end of the line, so a shorter line leaves nothing
        // of the longer one behind it.
        self.echo.extend_from_slice(b"\x1b[K");
        self.echo.extend_from_slice(&self.pending);
        let back = self.pending.len() - self.cursor;
        self.echo_left(back);
    }

    // --- escape sequences ---

    fn esc_byte(&mut self, byte: u8) {
        match self.esc {
            Esc::Ground => {}
            Esc::Start => match byte {
                b'[' => {
                    self.esc = Esc::Csi;
                    self.csi.clear();
                }
                b'O' => self.esc = Esc::Ss3,
                b']' => self.esc = Esc::Osc,
                // A character-set choice: `ESC ( B`. One byte, no meaning here.
                b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => self.esc = Esc::One,
                b'P' | b'X' | b'^' | b'_' => self.esc = Esc::Osc,
                // `ESC ESC` starts over, anything else is not a sequence after
                // all: drop it and carry on with the next byte.
                b'\x1b' => {}
                _ => self.esc = Esc::Ground,
            },
            Esc::Csi => match byte {
                // Parameters and intermediates: `1`, `;`, `5`, `?`.
                0x20..=0x3f => {
                    if self.csi.len() < CSI_MAX {
                        self.csi.push(byte);
                    }
                }
                // The final byte ends the sequence and says what key it was.
                0x40..=0x7e => {
                    let params = core::mem::take(&mut self.csi);
                    self.esc = Esc::Ground;
                    if let Some(key) = Self::csi_key(byte, &params) {
                        self.apply(key);
                    }
                }
                // A control byte mid-sequence: the sequence is nonsense.
                _ => {
                    self.csi.clear();
                    self.esc = Esc::Ground;
                }
            },
            Esc::Ss3 => {
                self.esc = Esc::Ground;
                if let Some(key) = Self::ss3_key(byte) {
                    self.apply(key);
                }
            }
            Esc::Osc => {
                // Ended by BEL, or by `ESC \`.
                if byte == 0x07 {
                    self.esc = Esc::Ground;
                } else if byte == b'\x1b' {
                    self.esc = Esc::One;
                }
            }
            Esc::One => self.esc = Esc::Ground,
        }
    }

    /// What key a `CSI` sequence was, if it was a key at all.
    ///
    /// A modifier comes as an extra parameter - `ESC [ 1 ; 5 C` is Ctrl-right -
    /// and is ignored: a shifted arrow is still the arrow, which is what
    /// whoever held the modifier meant to press.
    fn csi_key(final_byte: u8, params: &[u8]) -> Option<Key> {
        let (first, _modifier) = Self::params(params);
        match final_byte {
            b'A' => Some(Key::Up),
            b'B' => Some(Key::Down),
            b'C' => Some(Key::Right),
            b'D' => Some(Key::Left),
            b'E' => Some(Key::Down),
            b'H' => Some(Key::Home),
            b'F' => Some(Key::End),
            b'P' => Some(Key::Delete),
            b'~' => match first {
                1 | 7 => Some(Key::Home),
                3 => Some(Key::Delete),
                4 | 8 => Some(Key::End),
                5 => Some(Key::PageUp),
                6 => Some(Key::PageDown),
                _ => None,
            },
            // Everything else - a cursor position report, a mode set, a
            // question answered - is not a key, and is not typed either.
            _ => None,
        }
    }

    /// What key an `ESC O` sequence was: the same arrows, in the mode an
    /// application asks a terminal to use.
    fn ss3_key(byte: u8) -> Option<Key> {
        match byte {
            b'A' => Some(Key::Up),
            b'B' => Some(Key::Down),
            b'C' => Some(Key::Right),
            b'D' => Some(Key::Left),
            b'H' => Some(Key::Home),
            b'F' => Some(Key::End),
            _ => None,
        }
    }

    /// The first and last numeric parameters of a sequence. A parameter left
    /// out - `ESC [ ; 5 A` - means its default, which for these is 1, and a
    /// bare `ESC [ A` is the same as `ESC [ 1 A`.
    fn params(bytes: &[u8]) -> (usize, usize) {
        let mut nums = [1usize; 4];
        let mut count = 0usize;
        let mut cur: Option<usize> = None;
        for &b in bytes {
            if b.is_ascii_digit() {
                let d = (b - b'0') as usize;
                cur = Some(cur.unwrap_or(0) * 10 + d);
            } else if count < nums.len() {
                nums[count] = cur.take().unwrap_or(1);
                count += 1;
            } else {
                cur = None;
            }
        }
        if count < nums.len() {
            nums[count] = cur.take().unwrap_or(1);
            count += 1;
        }
        let first = nums[0];
        let last = nums[count.saturating_sub(1)];
        (first, if last == 0 { 1 } else { last })
    }

    fn apply(&mut self, key: Key) {
        match key {
            Key::Up => {
                self.history_prev();
            }
            Key::Down => {
                self.history_next();
            }
            Key::Left => {
                self.go_left();
            }
            Key::Right => {
                self.go_right();
            }
            Key::Home => {
                self.go_home();
            }
            Key::End => {
                self.go_end();
            }
            Key::Delete => self.delete_at_cursor(),
            // No scrolling buffer to page through yet.
            Key::PageUp | Key::PageDown => {}
        }
    }

    // --- the reader's side ---

    /// Record what went out on the console, so a redrawn input line can be put
    /// back where the prompt left it.
    ///
    /// Only what is printed while nothing is being typed is worth keeping:
    /// once there is a line being edited, the rest of the output line is the
    /// editor's own echo and would be replayed twice.
    ///
    /// Returns what `write_at` should put on the wire instead of the caller's
    /// buffer: the same bytes minus the device-status questions this line
    /// discipline answers itself. `None` - the common case - means forward
    /// everything untouched.
    pub fn note_output(&mut self, bytes: &[u8]) -> Option<alloc::vec::Vec<u8>> {
        // A question the discipline answers itself must not also reach the
        // wire: whatever real terminal sits beyond it would answer too, and
        // that second answer comes back as typed input - the ghost `R` a
        // fresh prompt showed, parsed and half-echoed by the shell.
        let mut swallowed: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
        // Where the sequence in flight began: byte 0 when it began in an
        // earlier call, the ESC's own index once this call has seen it.
        let mut seq_start = if self.out_esc != Esc::Ground {
            Some(0usize)
        } else {
            None
        };
        for (i, &b) in bytes.iter().enumerate() {
            match (self.out_esc, b) {
                // An OSC/DCS string in flight. BEL ends an OSC only (a DCS
                // keeps buffering); `ESC \` ends either, handled via `Esc::One`.
                (Esc::Osc, 0x07) => {
                    if self.out_dcs {
                        if self.out_osc.len() < OSC_MAX {
                            self.out_osc.push(b);
                        }
                    } else {
                        self.finish_osc(seq_start, i + 1, &mut swallowed);
                    }
                }
                (Esc::Osc, 0x1b) => {
                    // The `ESC` of a possible `ESC \` (ST): stay in the string
                    // and let the next byte decide between ST and a new sequence.
                    self.out_esc = Esc::One;
                }
                (Esc::Osc, _) => {
                    // Keep the head of the body (enough to spot `1+q`); a longer
                    // run is still a string we must finish on ST, so do not drop
                    // out of the state - just stop recording.
                    if self.out_osc.len() < OSC_MAX {
                        self.out_osc.push(b);
                    }
                }
                (_, 0x1b) => {
                    self.out_esc = Esc::Start;
                    self.out_params.clear();
                    seq_start = Some(i);
                }
                (Esc::Start, b'[') => {
                    self.out_esc = Esc::Csi;
                    self.out_params.clear();
                }
                (Esc::Start, b']') => {
                    self.out_esc = Esc::Osc;
                    self.out_osc.clear();
                    self.out_dcs = false;
                    self.in_string_seq = true;
                }
                // `ESC P` (DCS, e.g. XTGETTCap) and `ESC X` (SOS) are strings
                // that end on ST, never on BEL.
                (Esc::Start, b'P' | b'X') => {
                    self.out_esc = Esc::Osc;
                    self.out_osc.clear();
                    self.out_dcs = true;
                    self.in_string_seq = true;
                }
                // A character-set choice: `ESC ( B`, one byte more.
                (Esc::Start, 0x20..=0x2f) => self.out_esc = Esc::One,
                (Esc::Start, _) => self.out_esc = Esc::Ground,
                // Parameters and intermediates; the sequence ends on the
                // final byte, which is anything from `@` to `~`.
                (Esc::Csi, 0x20..=0x3f) => {
                    if self.out_params.len() < CSI_MAX {
                        self.out_params.push(b);
                    }
                }
                (Esc::Csi, _) => {
                    let params = core::mem::take(&mut self.out_params);
                    self.out_esc = Esc::Ground;
                    // A question this discipline answers itself is swallowed,
                    // so the host terminal beyond it cannot give a second
                    // answer that would come back as typed input: DSR (`5n`/
                    // `6n`) and Device Attributes (`c`, primary or `>c`
                    // secondary).
                    let answered = b == b'n' && matches!(params.first(), Some(b'5') | Some(b'6'));
                    if (answered || b == b'c')
                        && let Some(start) = seq_start
                    {
                        swallowed.push((start, i + 1));
                    }
                    seq_start = None;
                    self.output_csi(b, &params);
                }
                (Esc::One, b'\\') if self.in_string_seq => {
                    // `ESC \` is the ST terminator of the string in flight.
                    if self.out_dcs {
                        self.finish_dcs(seq_start, i + 1, &mut swallowed);
                    } else {
                        self.finish_osc(seq_start, i + 1, &mut swallowed);
                    }
                }
                (Esc::One, _) => {
                    self.out_esc = Esc::Ground;
                    self.in_string_seq = false;
                    self.out_dcs = false;
                }
                (Esc::Ground, LF) => {
                    self.prompt.clear();
                    self.screen.newline();
                }
                (Esc::Ground, CR) => {
                    self.prompt.clear();
                    self.screen.col = 0;
                }
                (Esc::Ground, 0x08) => {
                    self.prompt.pop();
                    self.screen.left(1);
                }
                (Esc::Ground, 0x09) => self.screen.tab(),
                // A bell says nothing and moves nothing.
                (Esc::Ground, 0x07) => {}
                (Esc::Ground, _) if self.pending.is_empty() && self.prompt.len() < PROMPT_MAX => {
                    self.prompt.push(b);
                    self.screen.put();
                }
                _ => self.screen.put(),
            }
        }
        if swallowed.is_empty() {
            return None;
        }
        let mut out = alloc::vec::Vec::with_capacity(bytes.len());
        let mut at = 0usize;
        for (start, end) in swallowed {
            out.extend_from_slice(&bytes[at..start]);
            at = end;
        }
        out.extend_from_slice(&bytes[at..]);
        Some(out)
    }

    /// One finished output sequence: move the cursor this line discipline is
    /// pretending to have, or answer the question the sequence asked.
    fn output_csi(&mut self, final_byte: u8, params: &[u8]) {
        let (first, second) = Self::params(params);
        match final_byte {
            b'A' => self.screen.up(first),
            b'B' | b'e' => self.screen.down(first),
            b'C' | b'a' => self.screen.right(first),
            b'D' => self.screen.left(first),
            b'E' => {
                self.screen.down(first);
                self.screen.col = 0;
            }
            b'F' => {
                self.screen.up(first);
                self.screen.col = 0;
            }
            b'G' | b'`' => self.screen.col = first.saturating_sub(1),
            b'd' => self.screen.row = first.saturating_sub(1),
            b'H' | b'f' => {
                self.screen.row = first.saturating_sub(1);
                self.screen.col = second.saturating_sub(1);
            }
            // Erasing. `2J` clears the screen and homes the cursor, which is
            // what a `clear` sends and what `^L` asks for.
            b'J' => {
                if first >= 2 {
                    self.screen.row = 0;
                    self.screen.col = 0;
                }
            }
            b'K' => {}
            b'n' => {
                // A question. `5n` asks whether the terminal is there; `6n`
                // asks where the cursor is, and a shell waits for the answer
                // before it reads another key - so the answer goes back the
                // way a terminal's would: as input.
                if first == 5 {
                    self.answer(b"\x1b[0n");
                } else if first == 6 {
                    self.answer(b"\x1b[");
                    let row = Self::decimal(self.screen.row + 1);
                    self.answer(&row);
                    self.answer(b";");
                    let col = Self::decimal(self.screen.col + 1);
                    self.answer(&col);
                    self.answer(b"R");
                }
            }
            // Device Attributes. A program that cannot consult a terminfo file
            // - `set_color`'s capability probe, a shell choosing its theme -
            // asks the terminal what it is by sending `CSI c` (primary) or
            // `CSI > c` (secondary) and reading the reply. Answered here, and
            // swallowed from the wire below, so the host does not also reply and
            // have its answer land as typed input.
            b'c' => {
                if params.contains(&b'>') {
                    // Secondary: terminal type 0, a fixed firmware version.
                    self.answer(b"\x1b>0;10;0c");
                } else {
                    // Primary: a VT220-class terminal with the common
                    // rectangular/highlight/cursor-ext extensions advertised.
                    self.answer(b"\x1b[?62;1;2;6;7;8;9c");
                }
            }
            // Everything else - colours, modes, scrolling regions - is drawn
            // by the host terminal and costs the cursor nothing.
            _ => {}
        }
    }

    /// Reply the way a terminal replies: on the input side.
    fn answer(&mut self, bytes: &[u8]) {
        self.answer.extend(bytes.iter().copied());
    }

    /// An OSC string ended (on BEL). If its body was a colour *query*, answer
    /// it and record the bytes for swallowing so the host cannot answer a
    /// second time; otherwise leave it to be forwarded as a colour set.
    fn finish_osc(
        &mut self,
        seq_start: Option<usize>,
        end: usize,
        swallowed: &mut alloc::vec::Vec<(usize, usize)>,
    ) {
        self.out_esc = Esc::Ground;
        self.in_string_seq = false;
        self.out_dcs = false;
        let payload = core::mem::take(&mut self.out_osc);
        let (index, rgb) = match osc_color_query(&payload) {
            Some(v) => v,
            None => return,
        };
        // `\e] <index> ; <rgb> \e\` - the reply a terminal gives a colour query.
        self.answer(b"\x1b]");
        self.answer(&Self::decimal(index));
        self.answer(b";");
        self.answer(rgb.as_bytes());
        self.answer(b"\x1b\\");
        if let Some(start) = seq_start {
            swallowed.push((start, end));
        }
    }

    /// A DCS string ended (on ST). A capability query (`ESC P 1 + q <hex>`,
    /// XTGETTCap - what fish/neovim/tmux use to ask a terminal its dynamic
    /// capabilities) is answered with an empty reply and swallowed, so the
    /// caller stops waiting and the host cannot answer a second time. Any other
    /// DCS is left to be forwarded.
    fn finish_dcs(
        &mut self,
        seq_start: Option<usize>,
        end: usize,
        swallowed: &mut alloc::vec::Vec<(usize, usize)>,
    ) {
        self.out_esc = Esc::Ground;
        self.in_string_seq = false;
        self.out_dcs = false;
        let body = core::mem::take(&mut self.out_osc);
        if body.starts_with(b"1+q") {
            self.answer(b"\x1bP1+r\x1b\\");
            if let Some(start) = seq_start {
                swallowed.push((start, end));
            }
        }
    }

    pub fn termios(&self) -> Termios {
        self.tio
    }

    pub fn set_termios(&mut self, tio: Termios) {
        let was_canonical = self.tio.lflag & ICANON != 0;
        self.tio = tio;
        // A sequence in flight belonged to the settings that were.
        self.esc = Esc::Ground;
        self.csi.clear();
        self.literal = false;
        // What was being typed belongs to whoever reads next, not to the mode
        // that was: a shell switching out of canonical mode mid-keystroke
        // would otherwise leave the start of the line behind it, and read a
        // command with its first characters missing.
        if was_canonical && !self.pending.is_empty() {
            self.flush();
        }
    }

    /// The echo queued since the last drain.
    pub fn take_echo(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.echo)
    }

    pub fn has_ready(&self) -> bool {
        if !self.answer.is_empty() || self.eof {
            return true;
        }
        if self.tio.lflag & ICANON != 0 {
            !self.out.is_empty()
        } else {
            // Raw, and `VMIN` is how many bytes a read waits for. One is the
            // usual answer, and zero would make every read return nothing at
            // all, which reads as end-of-file, so one it is.
            self.out.len() >= self.tio.cc[VMIN].max(1) as usize
        }
    }

    /// Take what a read asked for: at most `want` bytes, and in canonical
    /// mode at most one line, because that is what a canonical read means.
    ///
    /// `None` means nothing is ready yet - not the same as the empty slice,
    /// which is the end of input. The end of input comes last: `^D` after a
    /// half-typed line hands the line over first, and reports the end only
    /// once there is nothing left to read.
    pub fn take(&mut self, want: usize) -> Option<Vec<u8>> {
        // An answer is read on its own: never together with what was typed
        // after it, which a greedy reader would take and then lose.
        if !self.answer.is_empty() {
            let n = self.answer.len().min(want);
            return Some(self.answer.drain(..n).collect());
        }
        if self.out.is_empty() {
            if self.eof {
                self.eof = false;
                return Some(Vec::new());
            }
            return None;
        }
        let take = if self.tio.lflag & ICANON != 0 {
            // Up to and including the newline, but never more than asked for.
            let upto = self
                .out
                .iter()
                .position(|&b| b == LF)
                .map_or(self.out.len(), |i| i + 1);
            upto.min(want)
        } else {
            self.out.len().min(want)
        };
        Some(self.out.drain(..take).collect())
    }

    /// The line being edited, for tests and for anyone who must draw it.
    #[cfg(test)]
    fn line(&self) -> Vec<u8> {
        self.pending.clone()
    }
}

/// Where a terminal's output goes.
///
/// An enum rather than a trait object because a tty is built in a `const`
/// context, before there is an allocator. The real wires are the UART and the
/// master side of a pty pair; `Discard` is a terminal with nothing behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtyOutput {
    /// COM1 - the kernel console, `/dev/console` and `ttyS0`.
    Serial,
    /// A pty pair, by number: what the slave prints is what the master reads.
    Pty(u32),
    /// Nothing: the bytes are dropped.
    Discard,
}

/// One terminal: a line discipline, the readers waiting on it, and the wire its
/// output goes out.
///
/// There is more than one, and that is the point. The kernel's own voice is
/// `ttyS0` on COM1; the shells talk through the slave ends of pty pairs, and
/// the console multiplexer carries their traffic over the same serial wire.
/// Each terminal has its own line discipline, so a boot log and a login
/// prompt never edit the same buffer - and each has its own device number,
/// because `ttyname(3)` tells two terminals apart by exactly that.
pub struct Tty {
    ld: Mutex<RefCell<LineDiscipline>>,
    /// Tasks blocked in [`Tty::read`]. A producer cannot name them, but the
    /// tty can: a completed line is worthless if the only reader stays asleep.
    waiters: Mutex<RefCell<Vec<TaskId>>>,
    /// Who an interrupt belongs to: the last task to have asked this tty for
    /// input, standing in for the foreground process group. There is no
    /// process group yet, and a keyboard cannot say whose job a Ctrl-C is
    /// aimed at, so the most recent reader is the best guess there is - and
    /// the shell, waiting on a child, is not it.
    foreground: Mutex<RefCell<Option<TaskId>>>,
    /// Where output goes: the echo of what is typed, and everything a
    /// `write(2)` sends.
    output: Mutex<RefCell<TtyOutput>>,
    /// What `st_rdev` reports.
    dev_id: (u32, u32),
}

impl Tty {
    pub const fn new(dev_id: (u32, u32), output: TtyOutput) -> Tty {
        Tty {
            ld: Mutex::new(RefCell::new(LineDiscipline::new())),
            waiters: Mutex::new(RefCell::new(Vec::new())),
            foreground: Mutex::new(RefCell::new(None)),
            output: Mutex::new(RefCell::new(output)),
            dev_id,
        }
    }

    /// Send bytes to this terminal's wire. Called for echo, for `write(2)`, and
    /// for the answers the line discipline gives a cursor query - so all three
    /// land on the same wire the terminal is attached to.
    fn write_out(&self, bytes: &[u8]) {
        let output = critical_section::with(|cs| *self.output.borrow(cs).borrow());
        #[cfg(target_os = "none")]
        match output {
            TtyOutput::Serial => crate::console::write_bytes(bytes),
            TtyOutput::Pty(number) => crate::vfs::pty::master_input(number, bytes),
            TtyOutput::Discard => {}
        }
        // On the host there is no console to write to, but the pty wire is
        // plain memory - the pair tests drive it exactly as the machine does.
        #[cfg(not(target_os = "none"))]
        if let TtyOutput::Pty(number) = output {
            crate::vfs::pty::master_input(number, bytes);
        }
    }

    /// Feed one byte from the input side, show its echo, and act on what the
    /// byte asked for: a completed line wakes the readers, and an interrupt
    /// is delivered to whoever is reading. Runs with interrupts off for the
    /// length of the edit, which is nothing next to the byte's arrival rate.
    pub fn receive_byte(&self, byte: u8) {
        let (echo, action, waiters, foreground) = critical_section::with(|cs| {
            let mut ld = self.ld.borrow(cs).borrow_mut();
            let action = ld.receive(byte);
            let echo = ld.take_echo();
            let waiters = if ld.has_ready() {
                core::mem::take(&mut *self.waiters.borrow(cs).borrow_mut())
            } else {
                Vec::new()
            };
            let foreground = *self.foreground.borrow(cs).borrow();
            (echo, action, waiters, foreground)
        });
        self.write_out(&echo);
        #[cfg(target_os = "none")]
        {
            for id in waiters {
                crate::sched::wake(id);
            }
            // The interrupt goes to the foreground *process group*, not to the
            // one program reading: a pipeline is three processes and one job,
            // and Ctrl-C is aimed at the job. It is also what keeps the shell
            // alive - a shell sitting in `wait` has left the foreground group,
            // so the signal never reaches it.
            if action == Action::Interrupt
                && let Some(pgid) = foreground
            {
                crate::sched::send_signal_group(pgid, SIGINT);
            }
        }
        #[cfg(not(target_os = "none"))]
        drop((action, waiters, foreground));
    }

    /// Remember which process group an interrupt belongs to.
    ///
    /// A shell claims this for each job it starts and takes it back when the
    /// job finishes - through `TIOCSPGRP`, exactly as on Linux. A `wait`ing
    /// shell is therefore not in the foreground group, and a Ctrl-C aimed at a
    /// running command does not reach it: that is the whole convention, and a
    /// shell that took the interrupt itself would die where a real one prints
    /// a new prompt.
    #[cfg(target_os = "none")]
    pub fn set_foreground(&self, pgid: Option<TaskId>) {
        critical_section::with(|cs| {
            *self.foreground.borrow(cs).borrow_mut() = pgid;
        });
    }

    /// Whether `id` is in the group an interrupt from this tty would reach.
    #[cfg(target_os = "none")]
    pub fn is_foreground(&self, id: TaskId) -> bool {
        let Some(pgid) = self.foreground() else {
            return false;
        };
        crate::sched::pgid_of(id) == Some(pgid)
    }

    /// The process group an interrupt would reach, if any.
    pub fn foreground(&self) -> Option<TaskId> {
        critical_section::with(|cs| *self.foreground.borrow(cs).borrow())
    }

    /// What the terminal is set to. `TCGETS` reads this.
    pub fn termios(&self) -> Termios {
        critical_section::with(|cs| self.ld.borrow(cs).borrow().termios())
    }

    /// Set what the terminal is. `TCSETS` and its two flushing cousins write
    /// this; there is no output queue to drain, so all three do the same.
    pub fn set_termios(&self, tio: Termios) {
        critical_section::with(|cs| self.ld.borrow(cs).borrow_mut().set_termios(tio));
    }

    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    pub(crate) fn has_ready(&self) -> bool {
        critical_section::with(|cs| self.ld.borrow(cs).borrow().has_ready())
    }

    /// Blocking read: in canonical mode at most one line, in raw mode as much
    /// as is there, and 0 once input has ended. Never errors; the size is
    /// what the syscall returns.
    #[cfg(target_os = "none")]
    pub fn read(&self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let id = crate::sched::current();
        critical_section::with(|cs| self.waiters.borrow(cs).borrow_mut().push(id));
        // While this task is reading, an interrupt typed at the console is
        // meant for its process group - not for whatever waits on it. A shell
        // that has claimed the terminal with `TIOCSPGRP` has already said who
        // that is; this is the same answer for a program that never asks.
        // The kernel's own boot task has no process group; recording its
        // id would name a group that can never be signalled, and every
        // later `tcgetpgrp` on this terminal would read the poison.
        let fg = crate::sched::pgid_of(id).unwrap_or(id);
        if fg != crate::sched::TaskId(0) {
            self.set_foreground(Some(fg));
        }
        let got = self.read_blocking(buf);
        critical_section::with(|cs| {
            self.waiters.borrow(cs).borrow_mut().retain(|&w| w != id);
        });
        got
    }

    /// The same contract without a scheduler. On the host, tty reads block by
    /// construction, so only the discipline itself is testable.
    #[cfg(not(target_os = "none"))]
    pub fn read(&self, buf: &mut [u8]) -> usize {
        let _ = buf;
        panic!("blocking tty reads need the kernel scheduler; test LineDiscipline instead");
    }

    /// Take what a read would take, without blocking - the pair tests on the
    /// host, which cannot park a task but still want to see a typed line
    /// arrive whole at the slave.
    #[cfg(all(test, not(target_os = "none")))]
    pub(crate) fn take_ready(&self, buf: &mut [u8]) -> Option<usize> {
        critical_section::with(|cs| {
            let mut ld = self.ld.borrow(cs).borrow_mut();
            let bytes = ld.take(buf.len())?;
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            Some(n)
        })
    }

    #[cfg(target_os = "none")]
    fn read_blocking(&self, buf: &mut [u8]) -> usize {
        loop {
            crate::sched::block_until(|| self.has_ready());
            let got = critical_section::with(|cs| self.ld.borrow(cs).borrow_mut().take(buf.len()));
            // `None` means another reader took it first; wait for the next one.
            if let Some(bytes) = got {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                return n;
            }
        }
    }
}

/// The machine's own terminal: the console - `ttyS0` on COM1, where every
/// kernel log goes and where the boot task's standard descriptors point. The
/// shells' terminals are not here: `tty1`..`tty6` are the slave ends of pty
/// pairs (`pty::init` mints them), and the console multiplexer carries their
/// traffic over this one serial wire.
static TTYS: Mutex<RefCell<Vec<Arc<Tty>>>> = Mutex::new(RefCell::new(Vec::new()));

/// The device numbers the terminals report, from Linux's `tty` major: minor 64
/// is the first serial port. `tty1`..`tty6` carry `(4, n)` instead, and a pty
/// slave `(136, n)` - `ttyname(3)` matches a descriptor to a `/dev` name by
/// exactly these numbers, so every terminal keeps its own.
const CONSOLE_DEV_ID: (u32, u32) = (4, 64);

impl Default for Tty {
    fn default() -> Tty {
        Tty::new(CONSOLE_DEV_ID, TtyOutput::Serial)
    }
}

/// Create the console terminal and the virtual consoles, and hand back the
/// console.
///
/// Called once, from the boot task. The console tty is what the kernel's own
/// standard descriptors point at; the pty pairs behind `/dev/tty1`..`tty6`
/// are minted here too, so those names lead to real terminals - each with its
/// own line discipline and its own wire - by the time an `inittab` respawns a
/// getty on one.
pub fn init() -> Arc<Tty> {
    critical_section::with(|cs| {
        let mut ttys = TTYS.borrow(cs).borrow_mut();
        if ttys.is_empty() {
            ttys.push(Arc::new(Tty::new(CONSOLE_DEV_ID, TtyOutput::Serial)));
        }
        ttys[0].clone()
    });
    super::pty::init();
    critical_section::with(|cs| TTYS.borrow(cs).borrow()[0].clone())
}

/// The console tty, for whoever feeds it input: the kernel log's own wire, and
/// the terminal the COM1 receiver feeds before the console multiplexer exists.
pub fn global() -> Arc<Tty> {
    critical_section::with(|cs| {
        TTYS.borrow(cs)
            .borrow()
            .first()
            .expect("ttys exist only after vfs::tty::init()")
            .clone()
    })
}

/// The terminal whose device id this is, if one lives: the console by its
/// `(4, 64)`, a virtual console by its `(4, n)`, a pty slave by its `(136, n)`.
///
/// `/dev/tty` resolves through exactly this: the opening process carries the
/// device id of the terminal it acquired with `TIOCSCTTY`, and the id has to
/// lead back to the one `Tty` behind it. An id nothing mints is no terminal
/// at all, which is [`VfsError::NoDevice`]'s business at the open.
pub fn by_dev_id(dev_id: (u32, u32)) -> Option<Arc<Tty>> {
    match dev_id {
        (4, 64) => Some(global()),
        // `(4, n)` is a pair's slave and `(136, n)` its number in `/dev/pts`:
        // one terminal, two names, one `Tty`.
        (4, n) | (136, n) => super::pty::lookup(n).map(|pty| pty.slave().clone()),
        _ => None,
    }
}

/// `/dev/tty`: the opening process's controlling terminal.
///
/// The name is not a device in its own right. It is whatever terminal this
/// process acquired with `TIOCSCTTY`, which is why every open of it can land
/// on a different terminal: the getty that claimed `tty1` opens its own line,
/// a shell on `tty3` opens a different one, and the same process's open after
/// a `setsid` fails. A process that has acquired none cannot open the name at
/// all - POSIX answers `ENXIO`, which a shell reads as "job control off"
/// rather than as somebody else's terminal. That reading is load-bearing: an
/// interactive shell probes `tcgetpgrp` on `/dev/tty` to decide whether it is
/// in the foreground, and a name that resolved to a terminal nobody owns
/// turned every probe into "background" - a stop signal to itself, and a
/// login that froze forever for want of a SIGCONT nobody would send.
pub struct ControllingTty;

impl Inode for ControllingTty {
    /// A terminal has no position - `lseek(2)` on one is `ESPIPE` - and
    /// saying otherwise is what makes a reader drop the bytes it meant to
    /// push back.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// Linux's own numbers for the name: character major 5, minor 0.
    fn rdev(&self) -> Option<(u32, u32)> {
        Some((5, 0))
    }

    /// Every open resolves to the caller's controlling terminal - a different
    /// terminal for every process that has bound one, which is what
    /// [`Inode::open_replacement`] exists for.
    #[cfg(target_os = "none")]
    fn open_replacement(&self) -> VfsResult<Option<Arc<dyn Inode>>> {
        let Some(dev_id) = crate::sched::controlling_tty_of_current() else {
            return Err(VfsError::NoDevice);
        };
        by_dev_id(dev_id)
            .map(|tty| Some(tty as Arc<dyn Inode>))
            .ok_or(VfsError::NoDevice)
    }

    /// No scheduler on the host, so no process to ask; the pair tests drive
    /// terminals directly rather than through this name.
    #[cfg(not(target_os = "none"))]
    fn open_replacement(&self) -> VfsResult<Option<Arc<dyn Inode>>> {
        Err(VfsError::NoDevice)
    }
}

impl Inode for Tty {
    /// See [`ControllingTty`]: no position, so `lseek` is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0,
            kind: NodeKind::CharDev,
            // A stream has no size; reporting one would be a lie.
            size: 0,
        }
    }

    /// Termios ioctls are answered by the device, and a descriptor only says
    /// which device it is by handing back the inode it points at.
    fn as_tty(&self) -> Option<&Tty> {
        Some(self)
    }

    /// The tty device number, as libc's `ttyname` matches descriptors by: it
    /// walks `/dev` fstat-ing every node and comparing this number. Every
    /// terminal carries its own - `ttyS0` is `(4, 64)`, `tty1` is `(4, 1)`, a
    /// pty slave is `(136, n)` - so the walk finds the name of the terminal it
    /// is actually holding.
    fn rdev(&self) -> Option<(u32, u32)> {
        Some(self.dev_id)
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            // A completed line is what a read takes. Bytes still being typed
            // are not readable yet: canonical mode holds them until Enter.
            read: self.has_ready(),
            write: true,
        }
    }

    /// Join the list `receive_byte` wakes when a line completes. Without this a
    /// `poll` on the console would sleep through the very keystrokes it is
    /// waiting for - the wake goes to this list, not to the sleep queue.
    fn poll_subscribe(&self, id: TaskId) {
        critical_section::with(|cs| {
            let mut waiters = self.waiters.borrow(cs).borrow_mut();
            if !waiters.contains(&id) {
                waiters.push(id);
            }
        });
    }

    fn poll_unsubscribe(&self, id: TaskId) {
        critical_section::with(|cs| {
            self.waiters.borrow(cs).borrow_mut().retain(|&w| w != id);
        });
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        Ok(self.read(buf))
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        // Output can be input: a shell that asks where the cursor is gets
        // its answer here, and a reader waiting on that answer has to be
        // woken exactly as if the bytes had been typed.
        let (forwarded, waiters) = critical_section::with(|cs| {
            let mut ld = self.ld.borrow(cs).borrow_mut();
            let forwarded = ld.note_output(buf);
            let waiters = if ld.has_ready() {
                core::mem::take(&mut *self.waiters.borrow(cs).borrow_mut())
            } else {
                Vec::new()
            };
            (forwarded, waiters)
        });
        // What the discipline answered itself does not travel on: the
        // question would only be asked twice, once of a real terminal
        // whose reply has nowhere useful to go.
        match forwarded {
            Some(bytes) => self.write_out(&bytes),
            None => self.write_out(buf),
        }
        #[cfg(target_os = "none")]
        for id in waiters {
            crate::sched::wake(id);
        }
        #[cfg(not(target_os = "none"))]
        drop(waiters);
        Ok(buf.len())
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{Action, Key, LineDiscipline};
    use crate::vfs::termios::{ECHO, ECHOCTL, ICANON, ICRNL, ISIG, VERASE, VMIN};

    fn fed(bytes: &[u8]) -> LineDiscipline {
        let mut ld = LineDiscipline::new();
        for &b in bytes {
            let action = ld.receive(b);
            assert_eq!(action, Action::None, "byte {b:#x} asked for an action");
        }
        ld
    }

    fn lines(ld: &mut LineDiscipline) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(bytes) = ld.take(64) {
            out.push(bytes);
        }
        out
    }

    #[test]
    fn typing_a_line_echoes_it_and_completes_on_newline() {
        let mut ld = fed(b"hi\r");
        assert_eq!(ld.take_echo(), b"hi\n");
        // CR is Enter; the completed line carries the newline a reader expects.
        assert_eq!(lines(&mut ld), vec![b"hi\n".to_vec()]);
        assert!(!ld.has_ready());
    }

    #[test]
    fn backspace_edits_the_pending_line_and_its_echo() {
        let mut ld = fed(b"ab\x7fc\n");
        assert_eq!(ld.take_echo(), b"ab\x08 \x08c\n");
        assert_eq!(lines(&mut ld), vec![b"ac\n".to_vec()]);

        // Backspacing an empty line edits nothing and echoes nothing.
        let mut ld = fed(b"\x08\n");
        assert_eq!(ld.take_echo(), b"\n");
        assert_eq!(lines(&mut ld), vec![b"\n".to_vec()]);
    }

    #[test]
    fn ctrl_d_on_an_empty_line_is_eof_and_mid_line_is_a_flush() {
        let mut ld = fed(b"hi\n\x04");
        assert_eq!(lines(&mut ld), vec![b"hi\n".to_vec(), Vec::new()]);

        // A flush carries no newline: the reader gets the half-typed line as-is.
        let mut ld = fed(b"hi\x04");
        assert_eq!(lines(&mut ld), vec![b"hi".to_vec()]);
        assert!(!ld.has_ready());
    }

    #[test]
    fn a_read_takes_at_most_one_line_and_longer_lines_continue() {
        let mut ld = fed(b"abcdef\n");
        assert_eq!(ld.take(3), Some(b"abc".to_vec()));
        assert!(ld.has_ready(), "the rest of the line is still queued");
        assert_eq!(ld.take(3), Some(b"def".to_vec()));
        assert_eq!(ld.take(3), Some(b"\n".to_vec()));
        assert_eq!(ld.take(3), None);
    }

    #[test]
    fn control_bytes_without_editing_meaning_are_dropped() {
        let mut ld = fed(b"\x01\x02x\n");
        // ^A and ^B move the cursor rather than being typed, and at the start
        // of an empty line there is nowhere to move, so nothing is echoed.
        assert_eq!(ld.take_echo(), b"x\n");
        assert_eq!(lines(&mut ld), vec![b"x\n".to_vec()]);
    }

    #[test]
    fn echo_drains_so_repeated_takes_do_not_repeat() {
        let mut ld = LineDiscipline::new();
        let _ = ld.receive(b'x');
        assert_eq!(ld.take_echo(), b"x");
        assert_eq!(ld.take_echo(), b"");
    }

    #[test]
    fn ctrl_c_asks_for_an_interrupt_and_never_enters_the_line() {
        let mut ld = LineDiscipline::new();
        let _ = ld.receive(b'h');
        let _ = ld.receive(b'i');

        // The half-typed line is left where it was, as a terminal leaves it
        // when the reader it belonged to is about to be killed.
        assert_eq!(ld.receive(0x03), Action::Interrupt);
        assert!(!ld.has_ready(), "an interrupt completes no line");
        assert_eq!(
            ld.take_echo(),
            b"hi^C\n",
            "the interrupt is shown, not typed"
        );

        // Typing on afterwards continues the same line, so a Ctrl-C that
        // nobody was around to receive costs nothing.
        let _ = ld.receive(b'!');
        let _ = ld.receive(b'\n');
        assert_eq!(lines(&mut ld), vec![b"hi!\n".to_vec()]);
    }

    // --- arrow keys and friends: the point of the whole exercise ---

    #[test]
    fn arrow_keys_are_one_key_each_and_not_four_characters() {
        let mut ld = fed(b"ls");
        for &b in b"\x1b[D" {
            assert_eq!(ld.receive(b), Action::None);
        }
        assert_eq!(
            ld.line(),
            b"ls".to_vec(),
            "left moved the cursor, not the text"
        );
        let _ = ld.receive(b'x');
        // Typed between the two characters: `lxs`.
        assert_eq!(ld.line(), b"lxs".to_vec());

        let _ = ld.receive(b'\n');
        assert_eq!(
            lines(&mut ld),
            vec![b"lxs\n".to_vec()],
            "no `[`, `D` or ESC reached the line"
        );
    }

    #[test]
    fn a_modified_arrow_still_just_moves_the_cursor() {
        // `ESC [ 1 ; 3 C`: Alt-right. The modifier is not typed either.
        let mut ld = fed(b"ab");
        for &b in b"\x1b[1;3C" {
            let _ = ld.receive(b);
        }
        let _ = ld.receive(b'!');
        assert_eq!(ld.line(), b"ab!".to_vec());
    }

    #[test]
    fn the_delete_key_removes_under_the_cursor() {
        let mut ld = fed(b"abc");
        for &b in b"\x1b[D\x1b[D" {
            let _ = ld.receive(b);
        }
        for &b in b"\x1b[3~" {
            let _ = ld.receive(b);
        }
        assert_eq!(ld.line(), b"ac".to_vec());
    }

    #[test]
    fn a_cursor_position_report_is_not_typed() {
        // A shell asks `ESC[6n`; the terminal answers `ESC[41;5R`. That answer
        // is for the shell, never for the line - and used to arrive as
        // `[41;5R` typed at the prompt.
        let mut ld = LineDiscipline::new();
        for &b in b"\x1b[41;5R" {
            assert_eq!(ld.receive(b), Action::None);
        }
        assert_eq!(ld.line(), Vec::<u8>::new());
        assert!(!ld.has_ready());
    }

    #[test]
    fn modified_and_ss3_arrows_are_still_arrows() {
        let mut ld = fed(b"ab");
        // Ctrl-left: `ESC [ 1 ; 5 D`.
        for &b in b"\x1b[1;5D" {
            let _ = ld.receive(b);
        }
        let _ = ld.receive(b'X');
        assert_eq!(ld.line(), b"aXb".to_vec());

        // Application cursor mode: `ESC O C` is right, `ESC O A` is up.
        let mut ld = fed(b"ab");
        for &b in b"\x1bOC" {
            let _ = ld.receive(b);
        }
        let _ = ld.receive(b'Z');
        assert_eq!(ld.line(), b"abZ".to_vec());
    }

    #[test]
    fn up_and_down_walk_the_history() {
        let mut ld = fed(b"one\n");
        let _ = lines(&mut ld);
        let mut ld = {
            ld.receive(b't');
            ld.receive(b'w');
            ld.receive(b'o');
            ld.receive(b'\n');
            ld
        };
        let _ = lines(&mut ld);
        let _ = ld.take_echo();

        // Up: the last line comes back.
        for &b in b"\x1b[A" {
            let _ = ld.receive(b);
        }
        assert_eq!(ld.line(), b"two".to_vec());

        // Up again: the one before it.
        for &b in b"\x1b[A" {
            let _ = ld.receive(b);
        }
        assert_eq!(ld.line(), b"one".to_vec());

        // Down twice returns to what was being typed: nothing.
        for &b in b"\x1b[B" {
            let _ = ld.receive(b);
        }
        assert_eq!(ld.line(), b"two".to_vec());
        for &b in b"\x1b[B" {
            let _ = ld.receive(b);
        }
        assert_eq!(ld.line(), Vec::<u8>::new());

        let _ = ld.receive(b'\n');
        assert_eq!(lines(&mut ld), vec![b"\n".to_vec()], "history is not input");
    }

    #[test]
    fn history_keeps_what_was_being_typed_when_up_was_pressed() {
        let mut ld = fed(b"first\n");
        let _ = lines(&mut ld);
        ld.receive(b'p');
        ld.receive(b'a');
        ld.receive(b'r');
        ld.receive(b't');
        for &b in b"\x1b[A" {
            ld.receive(b);
        }
        assert_eq!(ld.line(), b"first".to_vec());
        for &b in b"\x1b[B" {
            ld.receive(b);
        }
        assert_eq!(
            ld.line(),
            b"part".to_vec(),
            "down came back to the half-typed line"
        );
    }

    #[test]
    fn the_emacs_keys_edit_the_line() {
        // ^A home, ^E end, ^K kill to end, ^U kill whole line, ^W a word.
        let mut ld = fed(b"hello world");
        ld.receive(0x01); // ^A
        ld.receive(0x0e); // ^N: nothing below, no history yet
        assert_eq!(ld.line(), b"hello world".to_vec());

        let mut ld = fed(b"hello world");
        for _ in 0..5 {
            ld.receive(0x02); // ^B, back five
        }
        ld.receive(0x0b); // ^K
        assert_eq!(ld.line(), b"hello ".to_vec());

        let mut ld = fed(b"one two three");
        ld.receive(0x17); // ^W
        assert_eq!(ld.line(), b"one two ".to_vec());

        let mut ld = fed(b"anything");
        ld.receive(0x15); // ^U
        assert_eq!(ld.line(), Vec::<u8>::new());
    }

    // --- termios: what a shell asks for is what it gets ---

    fn raw_ld() -> LineDiscipline {
        let mut ld = LineDiscipline::new();
        let mut tio = ld.termios();
        tio.lflag &= !(ICANON | ECHO);
        tio.cc[VMIN] = 1;
        ld.set_termios(tio);
        ld
    }

    #[test]
    fn raw_mode_hands_over_every_byte_including_escape_sequences() {
        let mut ld = raw_ld();
        for &b in b"\x1b[A" {
            assert_eq!(ld.receive(b), Action::None);
        }
        // Not edited, not echoed, and the arrow is the reader's to interpret.
        assert_eq!(ld.take_echo(), b"");
        assert_eq!(ld.take(8), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn raw_mode_reads_are_not_held_until_a_newline() {
        let mut ld = raw_ld();
        ld.receive(b'l');
        assert!(ld.has_ready(), "one byte is enough when VMIN is one");
        assert_eq!(ld.take(8), Some(b"l".to_vec()));
        // ^D is an ordinary byte here, not the end of input.
        ld.receive(0x04);
        assert_eq!(ld.take(8), Some(b"\x04".to_vec()));
    }

    #[test]
    fn raw_mode_still_interrupts_on_ctrl_c() {
        let mut ld = raw_ld();
        assert!(ld.termios().lflag & ISIG != 0, "raw is not deaf to ^C");
        assert_eq!(ld.receive(0x03), Action::Interrupt);
        assert_eq!(ld.take(8), None, "the byte was not input");
    }

    #[test]
    fn turning_echo_off_stops_the_echo() {
        let mut ld = LineDiscipline::new();
        let mut tio = ld.termios();
        tio.lflag &= !ECHO;
        ld.set_termios(tio);
        ld.receive(b'x');
        ld.receive(b'\n');
        assert_eq!(ld.take_echo(), b"");
        assert_eq!(ld.take(8), Some(b"x\n".to_vec()));
    }

    #[test]
    fn echoctl_is_what_shows_the_interrupt_and_can_be_turned_off() {
        let mut ld = LineDiscipline::new();
        let mut tio = ld.termios();
        tio.lflag &= !ECHOCTL;
        ld.set_termios(tio);
        assert_eq!(ld.receive(0x03), Action::Interrupt);
        assert_eq!(ld.take_echo(), b"");
    }

    #[test]
    fn the_erase_character_is_what_termios_says_it_is() {
        let mut ld = LineDiscipline::new();
        let mut tio = ld.termios();
        tio.cc[VERASE] = 0x08;
        ld.set_termios(tio);
        ld.receive(b'a');
        ld.receive(b'b');
        ld.receive(0x08);
        assert_eq!(ld.line(), b"a".to_vec());
    }

    #[test]
    fn icrnl_makes_enter_a_newline_and_can_be_turned_off() {
        let mut ld = LineDiscipline::new();
        ld.receive(b'a');
        ld.receive(b'\r');
        assert_eq!(ld.take(8), Some(b"a\n".to_vec()));

        let mut ld = LineDiscipline::new();
        let mut tio = ld.termios();
        tio.iflag &= !ICRNL;
        ld.set_termios(tio);
        ld.receive(b'a');
        ld.receive(b'\r');
        // CR is a character of the line now, not its end: nothing is ready to
        // be read, which is exactly what `stty -icrnl` does to a terminal.
        assert!(!ld.has_ready());
        assert_eq!(ld.line(), b"a\r".to_vec());
        ld.receive(b'\n');
        assert_eq!(ld.take(8), Some(b"a\r\n".to_vec()));
    }

    #[test]
    fn the_prompt_is_remembered_and_replayed_when_the_line_is_redrawn() {
        let mut ld = LineDiscipline::new();
        ld.note_output(b"/ # ");
        ld.receive(b'l');
        ld.receive(b's');
        let _ = ld.take_echo();
        // A redraw has to put the line back after the prompt, not at column 0.
        ld.receive(0x15); // ^U
        assert_eq!(ld.take_echo(), b"\r/ # \x1b[K");
    }

    #[test]
    fn a_cursor_position_query_is_answered_on_the_input_side() {
        // What a shell sends before it draws anything, and waits for.
        let mut ld = LineDiscipline::new();
        ld.note_output(b"/ # ");
        ld.note_output(b"\x1b[6n");
        // One-based, and the cursor is four characters along: row 1, column 5.
        assert_eq!(ld.take(16), Some(b"\x1b[1;5R".to_vec()));

        // It is input, so a reader waiting for it is woken by it.
        assert!(!ld.has_ready(), "the answer is read once");
    }

    #[test]
    fn the_cursor_answers_where_it_actually_is() {
        let mut ld = LineDiscipline::new();
        ld.note_output(b"a line\n");
        ld.note_output(b"xy");
        ld.note_output(b"\x1b[6n");
        assert_eq!(ld.take(16), Some(b"\x1b[2;3R".to_vec()));
    }

    #[test]
    fn a_device_status_query_gets_the_terminals_ok() {
        let mut ld = LineDiscipline::new();
        ld.note_output(b"\x1b[5n");
        assert_eq!(ld.take(16), Some(b"\x1b[0n".to_vec()));
    }

    /// `set_color` and a shell picking its theme ask the terminal what it is
    /// with a Device Attributes query. It must be answered on the input side,
    /// and swallowed on the output side, so the host cannot answer a second
    /// time and have its reply arrive as typed input.
    #[test]
    fn a_device_attributes_query_is_answered_and_not_forwarded() {
        let mut ld = LineDiscipline::new();
        // Primary DA (`CSI c`): a VT220-class answer.
        let fwd = ld.note_output(b"\x1b[c");
        assert_eq!(ld.take(64), Some(b"\x1b[?62;1;2;6;7;8;9c".to_vec()));
        assert!(fwd.is_some() && fwd.unwrap().is_empty(), "the query is swallowed");

        // Secondary DA (`CSI > c`): the terminal-identity answer.
        let fwd = ld.note_output(b"\x1b[>c");
        assert_eq!(ld.take(64), Some(b"\x1b>0;10;0c".to_vec()));
        assert!(fwd.is_some() && fwd.unwrap().is_empty(), "the query is swallowed");
    }

    /// A colour (`CSI 32m`) ends in `m`, not `c`: it is drawn by the host and
    /// forwarded untouched - not mistaken for a Device Attributes query.
    #[test]
    fn a_colour_sequence_is_forwarded_not_answered() {
        let mut ld = LineDiscipline::new();
        assert!(ld.note_output(b"\x1b[32m").is_none(), "nothing to answer, so nothing swallowed");
        assert!(ld.take(16).is_none(), "no reply is queued");
    }

    /// `set_color` and a colour-theme probe ask the terminal its background
    /// colour with an OSC query. Left unanswered, the host replies and the
    /// reply arrives as typed input - the garbled bytes behind fish's
    /// `d=cyan` parse failure. Answered and swallowed here.
    #[test]
    fn a_background_colour_query_is_answered_and_not_forwarded() {
        let mut ld = LineDiscipline::new();
        let fwd = ld.note_output(b"\x1b]11;?\x07");
        assert_eq!(
            ld.take(64),
            Some(b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\".to_vec()),
            "a well-formed background reply"
        );
        assert!(fwd.is_some() && fwd.unwrap().is_empty(), "the query is swallowed");

        // A colour *set* (`11;rgb:...`) is not a question: forwarded, no reply.
        assert!(ld.note_output(b"\x1b]11;rgb:0000/0000/0000\x07").is_none());
        assert!(ld.take(64).is_none());
    }

    /// XTGETTCap (`ESC P 1 + q <hex> ESC \`) is the modern, *dynamic* terminfo
    /// probe - the one fish relies on above the static terminfo file. mpos must
    /// answer it itself and swallow the query, or the host's reply arrives as
    /// typed input and the theme parse breaks. An empty `1 + r` reply is a
    /// valid "no extra capabilities" answer that lets the caller move on.
    #[test]
    fn an_xtgettcap_query_is_answered_and_not_forwarded() {
        let mut ld = LineDiscipline::new();
        // A query for the capability `colors` (hex 636f6c6f7273), ST-terminated.
        let fwd = ld.note_output(b"\x1bP1+q636f6c6f7273\x1b\\");
        assert_eq!(
            ld.take(64),
            Some(b"\x1bP1+r\x1b\\".to_vec()),
            "an empty capability reply"
        );
        assert!(fwd.is_some() && fwd.unwrap().is_empty(), "the query is swallowed");
    }

    #[test]
    fn output_escape_sequences_are_not_mistaken_for_a_prompt() {
        let mut ld = LineDiscipline::new();
        ld.note_output(b"\x1b[32m");
        ld.note_output(b"$ ");
        ld.receive(b'x');
        let _ = ld.take_echo();
        ld.receive(0x15);
        assert_eq!(
            ld.take_echo(),
            b"\r$ \x1b[K",
            "the colour sequence is not part of the prompt"
        );
    }

    #[test]
    fn csi_parameters_are_read_from_both_ends() {
        assert_eq!(LineDiscipline::params(b""), (1, 1));
        assert_eq!(LineDiscipline::params(b"3"), (3, 3));
        assert_eq!(LineDiscipline::params(b"1;5"), (1, 5));
        assert_eq!(LineDiscipline::params(b";2"), (1, 2));
    }

    #[test]
    fn a_bare_escape_and_a_ruined_sequence_are_dropped() {
        let mut ld = LineDiscipline::new();
        ld.receive(b'\x1b');
        ld.receive(b'x'); // not a sequence after all: both bytes dropped
        assert_eq!(ld.line(), Vec::<u8>::new());

        // A sequence that never ends - too many parameters to be a key - is
        // given up on rather than left hanging over the next thing typed.
        let mut ld = LineDiscipline::new();
        for &b in b"\x1b[" {
            ld.receive(b);
        }
        for _ in 0..40 {
            ld.receive(b'9');
            ld.receive(b';');
        }
        ld.receive(b'A');
        assert_eq!(ld.line(), Vec::<u8>::new());
        // And the next line typed is unaffected.
        ld.receive(b'o');
        ld.receive(b'k');
        assert_eq!(ld.line(), b"ok".to_vec());
    }

    #[test]
    fn keys_are_decoded_from_their_sequences() {
        assert_eq!(LineDiscipline::csi_key(b'A', b""), Some(Key::Up));
        assert_eq!(LineDiscipline::csi_key(b'~', b"3"), Some(Key::Delete));
        assert_eq!(LineDiscipline::csi_key(b'~', b"5"), Some(Key::PageUp));
        assert_eq!(LineDiscipline::csi_key(b'R', b"41;5"), None);
        assert_eq!(LineDiscipline::ss3_key(b'H'), Some(Key::Home));
    }
}
