//! The tty: a line discipline between the machine's input and its readers.
//!
//! Bytes arrive one at a time - today from whoever calls [`Tty::receive_byte`],
//! tomorrow from a serial interrupt - and what happens to them depends on the
//! settings in [`Termios`]: in canonical mode they are edited into a line here
//! and a read takes a whole line; in raw mode they go straight to the reader,
//! which is how a shell does its own editing and how arrow keys reach it as
//! the sequences a terminal actually sent.
//!
//! Editing is backspace, line-kill and word-erase, and nothing more: a
//! cursor, a history the up arrow walks, an escape sequence recognised as one
//! key - all of that is a line editor's job, and the shells and editors here
//! (fish, vi) run the terminal in raw mode and bring their own. A canonical
//! reader on Linux gets exactly what was typed, control bytes and `ESC [ A`
//! included, delivered when the line is completed; this discipline does the
//! same, and echoes a control byte as its `^X` form the way `n_tty` does with
//! `ECHOCTL`. Sequences are input like any other character.
//!
//! Output is input's other half, and a question written *out* is answered here
//! only when this kernel is the terminal that owns the screen. Measured on Linux,
//! a pty pair's line discipline forwards `ESC [ 6 n` and its cousins byte for byte
//! and answers none of them, because the emulator holding the master has the
//! cursor; the console driver answers the same question for a VT, because there
//! the kernel is the one drawing it. [`Attached`] is that decision, and the
//! discipline's cursor is the [`crate::term::cursor`] the grid paints with, so
//! a report cannot describe a screen that is not the one being shown.
//!
//! [`LineDiscipline`] is the pure state machine and runs under `cargo test` on
//! the host. [`Tty`] owns one under a mutex, echoes to the console as bytes are
//! received, and turns `read` into a blocking, *interruptible* wait via
//! [`crate::sched::block_until_deadline`] - Linux's answer to a signal that a
//! process can catch is to end the read with `EINTR`, and the clock half of the
//! same wait is what `VMIN`/`VTIME` mean.
//!
//! The standard descriptors fd 0, 1 and 2 are three open descriptions of this
//! one inode, created at start-up (`vfs::file::init_standard`). A `/dev/tty`
//! node waits until fds are per-process (M12), when a shell can open one;
//! until then nothing user side names this device.

use alloc::borrow::Cow;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::termios::{
    ECHO, ECHOCTL, ICANON, ICRNL, IGNCR, INLCR, ISIG, ISTRIP, IXON, ONLCR, OPOST, Termios, VEOF,
    VERASE, VINTR, VKILL, VLNEXT, VMIN, VQUIT, VREPRINT, VSTART, VSTOP, VSUSP, VTIME, VWERASE,
};
use super::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
use crate::sched::policy::TaskId;
#[cfg(target_os = "none")]
use crate::sched::signal::{SIGINT, SIGQUIT, SIGTSTP, SIGWINCH};
use crate::term::cursor::Cursor;
use crate::term::vc::VC_COUNT;
/// The KD/K video+keyboard mode words are named only by `tty`'s `kd_ioctl` /
/// `vt_ioctl`, so the imports go with them - which is every machine now that the
/// console mux is both machines' (`PLAN.md` §13.16).
#[cfg(target_os = "none")]
use crate::term::vc::{K_MEDIUMRAW, K_OFF, K_RAW, K_UNICODE, K_XLATE, KD_GRAPHICS, KD_TEXT};

const LF: u8 = b'\n';
const CR: u8 = b'\r';
/// The other key terminals send for erase. `VERASE` is `0x7f`, but a terminal
/// in raw mode may send `^H` instead, and both mean the same thing to whoever
/// is typing.
const BACKSPACE: u8 = 0x08;

/// How many bytes of an escape sequence's parameters are gathered before the
/// sequence is given up as a mistake.
const CSI_MAX: usize = 16;
/// How many parameters one CSI is read for. `;`-separated beyond this is a
/// sequence nobody means, and the modes a `h`/`l` can carry fit inside it.
const PARAMS_MAX: usize = 8;
/// Cap on the body of an OSC string tracked on the output, so a colour query
/// can be recognised; a longer run is a colour *set*, which is just forwarded.
const OSC_MAX: usize = 32;
/// How long a held output sequence may grow before it is given up as a
/// sequence and put on the wire as ordinary text. A program that writes an
/// unterminated `ESC [` must not lose everything after it.
const HELD_MAX: usize = 64;

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
/// Whether the kernel console's own voice - `TtyOutput::Serial`, `/dev/console`
/// and `ttyS0` - is being painted on a grid the kernel owns.
///
/// `console::write_bytes` feeds the active grid while one exists and the serial
/// wire when it does not, so this is the whole of the question "is there a
/// terminal behind these bytes that can answer for itself". The module that
/// knows is bare-metal only, and on the host nothing is ever painted, which is
/// also the honest answer there.
#[cfg(target_os = "none")]
fn kernel_console_has_grids() -> bool {
    crate::console::grids_live()
}

#[cfg(not(target_os = "none"))]
fn kernel_console_has_grids() -> bool {
    false
}

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
    /// The foreground reader is to be told to quit — the terminal's `VQUIT`
    /// character, `SIGQUIT` to the foreground group.
    Quit,
    /// The foreground reader is to be suspended — the terminal's `VSUSP`
    /// character, `SIGTSTP` to the foreground group, the one a job-control
    /// shell then reports as `[1]+ Stopped` and brings back with `fg`.
    Suspend,
}

/// Where an output escape sequence has got to. Anything other than
/// [`Ground`](Self::Ground) means bytes of a sequence are being gathered on
/// the way *out*, held from the wire until the sequence resolves. Input has no
/// such state: what a shell types is characters, all of them, like `n_tty`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Esc {
    Ground,
    /// Saw `ESC`.
    Start,
    /// Saw `ESC [`: control sequence, parameters being gathered.
    Csi,
    /// Saw `ESC ]`: an operating-system command, ended by BEL.
    Osc,
    /// A sequence that swallows exactly one more byte - a character-set
    /// choice, or a string control's terminator.
    One,
}

/// The terminal a discipline's output is attached to: who draws it, and how big
/// it is.
///
/// Both halves exist because the line discipline used to guess at them.
///
/// `kernel_draws` is the answer to the one question that decides whether an
/// `ESC [ 6 n` is answered here or sent on its way: *is this kernel the terminal
/// emulator for these bytes?* Measured on Linux, a pty pair's line discipline
/// interprets nothing on the output side at all - `ESC [ 6 n`, `ESC [ 5 n`,
/// `ESC [ c`, `ESC [ ? 6 n`, an `OSC 11 ; ?` and a `DCS 1 + q` each arrived at the
/// master byte for byte, and none of them was answered: `/dev/pts` is a wire, and
/// the thing at the other end of it is the terminal. The console driver answers
/// those same questions for a VT, because there the kernel *is* the emulator.
/// This kernel answered them everywhere, and so told a shell sitting on a serial
/// line about a cursor it could not see: `fish` pads the line it is about to
/// overwrite out to the width it was told, and the wrap that would have preserved
/// the command's output with it.
///
/// `rows`/`cols` are the size that same terminal reports to `TIOCGWINSZ`, and they
/// are taken here because a cursor counted against one size while a window is
/// reported at another tells a program twice. Measured with `stty cols 40` on a
/// shell whose cursor report was still being computed against `80`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attached {
    pub kernel_draws: bool,
    pub rows: usize,
    pub cols: usize,
}

impl Attached {
    /// A terminal the kernel neither draws nor owns the size of: what a `/dev/pts`
    /// pair looks like to its line discipline, and the shape host tests that do
    /// not care about the answering half use.
    pub const fn downstream() -> Attached {
        Attached {
            kernel_draws: false,
            rows: 25,
            cols: 80,
        }
    }

    /// The kernel is the emulator, at this size.
    pub const fn kernel(rows: usize, cols: usize) -> Attached {
        Attached {
            kernel_draws: true,
            rows,
            cols,
        }
    }
}

/// How a blocking non-canonical read waits, as [`LineDiscipline::read_wait`]
/// decides it.
///
/// The four states are the four answers a program can branch on, and they are
/// not interchangeable: `Empty` and an end of input both hand the caller zero
/// bytes, but only one of them says the terminal closed, so they travel by
/// different doors (`Empty` is reported by the read itself, an end of input by
/// [`LineDiscipline::take`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadWait {
    /// There is something to take: a queued answer, an end of input, or the
    /// `VMIN` bytes the read asked for.
    Ready,
    /// Take nothing, wait for nothing, answer 0 bytes now: the `VMIN == 0` with
    /// `VTIME == 0` poll, whose empty queue is a complete answer.
    Empty,
    /// Wait for input with no clock. Ends when a read decision becomes
    /// available, or a signal interrupts it.
    UntilInput,
    /// Wait until this tick, which is the read's `VTIME` clock running out.
    /// The caller compares it with its own clock and re-asks on every pass,
    /// because an arriving byte moves it.
    UntilTick(u64),
    /// Take nothing and wait for nothing: `O_NONBLOCK` on a read whose answer
    /// would have been a wait. The one wait it does *not* replace is
    /// [`Self::Empty`], which never waited at all.
    WouldBlock,
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
    /// The line being typed, appended to at its end until it is completed.
    /// `n_tty` has no cursor into this buffer and neither does this one - the
    /// keys that move one belong to a line editor, in raw mode, in userspace.
    pending: Vec<u8>,
    /// What the console should show, since the last drain.
    echo: Vec<u8>,
    /// The settings that decide everything above.
    tio: Termios,
    /// Taken literally rather than as a control character: set by `^V`.
    literal: bool,
    /// Where an escape sequence in the *output* has got to, so the cursor
    /// model swallows the whole sequence instead of mistaking a stray `[` for
    /// a column.
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
    /// The bytes of the output sequence in flight, held back from the wire.
    /// Whether a sequence is a question this discipline answers is only known
    /// when it ends, and what has already reached the wire cannot be taken
    /// back - so nothing of it goes out until it resolves. See
    /// [`LineDiscipline::note_output`].
    out_held: Vec<u8>,
    /// Where the cursor stands, on the screen the kernel draws when it is the
    /// terminal. Shared type with the renderer, so a report cannot describe a
    /// cursor the screen does not have.
    screen: Cursor,
    /// The main screen's cursor, parked while an alternate screen is showing
    /// (`CSI ? 47 / 1047 / 1049 h`), which is what `less` and `vi` leave behind
    /// them. Kept apart from [`LineDiscipline::screen`] because the screen it
    /// belongs to is not the one being counted right now.
    alt_cursor: Option<crate::term::cursor::Snapshot>,
    /// The tick the last input byte landed, kept only for a non-canonical
    /// read's `VTIME` clock - which Linux measures *between bytes*, not from
    /// the moment the read asked (measured: `VMIN=2/VTIME=5` with one byte
    /// arriving 100 ms in returns that byte 500 ms after it landed, not 500 ms
    /// after the read began). The discipline stores the number rather than
    /// reading a clock, so the whole wait policy stays host-testable; a host
    /// that never feeds it a tick sees 0.
    last_byte: u64,
    /// How many input bytes have landed. A blocking non-canonical read whose
    /// `VTIME` clock is started by a byte parks on this count changing, not only
    /// on a completed read: one partial byte does not satisfy `VMIN`, so the
    /// wake that re-anchors the inter-byte clock would otherwise be swallowed by
    /// `block_until_deadline`'s condition loop and the read would sleep forever.
    /// See the read-path note in `PLAN.md` §12.4a.
    input_count: u64,
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
            echo: Vec::new(),
            tio: Termios::console_default(),
            literal: false,
            out_esc: Esc::Ground,
            out_params: Vec::new(),
            out_osc: Vec::new(),
            out_dcs: false,
            in_string_seq: false,
            out_held: Vec::new(),
            screen: Cursor::new(25, 80),
            alt_cursor: None,
            last_byte: 0,
            input_count: 0,
        }
    }

    /// Feed one input byte, queueing whatever the console should echo, and
    /// report anything the byte asked for beyond editing.
    pub fn receive(&mut self, byte: u8) -> Action {
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
            // The other two ISIG keys. Both go to the foreground group like
            // the interrupt does; what differs is only the signal they carry,
            // and the disposition decides the rest: a process that ignores or
            // catches `SIGTSTP` runs on (vim's `mch_suspend` relies on
            // exactly that to suspend itself in two steps), while the default
            // action — a stop the shell sees through `WUNTRACED` — is what
            // makes Ctrl-Z the keyboard's "put this job in the background".
            if byte == self.tio.cc[VQUIT] {
                self.echo_control(b'^', b'\\');
                return Action::Quit;
            }
            if byte == self.tio.cc[VSUSP] {
                self.echo_control(b'^', b'Z');
                return Action::Suspend;
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
                if self.echo_on() {
                    self.echo.push(CR);
                    self.echo.extend_from_slice(b"\x1b[K");
                    self.echo.extend_from_slice(&self.pending);
                }
                Action::None
            }
            b if b == lnext => {
                self.literal = true;
                Action::None
            }
            // Everything else is a character of the line, control bytes
            // included - `ESC`, `^A`, `^T`. `n_tty` claims exactly the termios
            // specials above and types the rest, and a canonical reader that
            // wants an arrow key's bytes reads them off the line like anyone
            // else. A shell that edits brings its own editor, in raw mode.
            b => {
                self.insert(b);
                Action::None
            }
        }
    }

    // --- editing the line ---

    /// Type one byte at the end of the line, and show it. A control byte is
    /// shown as its `^X` form when `ECHOCTL` is on, the way `n_tty`'s
    /// `echo_char` does: an `ESC` typed at a canonical prompt leaves `^[` on
    /// the screen and the byte itself in the line.
    fn insert(&mut self, byte: u8) {
        self.pending.push(byte);
        if !self.echo_on() {
            return;
        }
        if byte != b'\t' && byte != LF && (byte < 0x20 || byte == 0x7f) {
            if self.tio.lflag & ECHOCTL != 0 {
                self.echo.push(b'^');
                self.echo.push(byte | 0x40);
            }
        } else {
            self.echo.push(byte);
        }
    }

    /// `VERASE`: chop the last character off the line. There is no cursor to
    /// delete behind, because there is no way to move one.
    fn backspace(&mut self) {
        if self.pending.pop().is_some() && self.echo_on() {
            // Erase on a terminal: step back, blank, step back again.
            self.echo.extend_from_slice(b"\x08 \x08");
        }
    }

    /// `VKILL`: the whole line, each character erased the way `^H` erases
    /// one (`ECHOKE`'s rendering in `n_tty`).
    fn kill_line(&mut self) {
        let n = self.pending.len();
        if n == 0 {
            return;
        }
        self.pending.clear();
        if self.echo_on() {
            for _ in 0..n {
                self.echo.extend_from_slice(b"\x08 \x08");
            }
        }
    }

    /// `^W`: the word behind the end of the line, and the spaces in front of
    /// it.
    fn erase_word(&mut self) {
        let mut end = self.pending.len();
        while end > 0 && self.pending[end - 1] == b' ' {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && self.pending[start - 1] != b' ' {
            start -= 1;
        }
        let erased = self.pending.len() - start;
        self.pending.truncate(start);
        if self.echo_on() {
            for _ in 0..erased {
                self.echo.extend_from_slice(b"\x08 \x08");
            }
        }
    }

    // --- completing input ---

    fn complete_line(&mut self) {
        self.pending.push(LF);
        if self.echo_on() {
            self.echo.push(LF);
        }
        let line = core::mem::take(&mut self.pending);
        self.out.extend(line.iter().copied());
    }

    /// `^D` mid-line: hand over what is typed, without a newline.
    fn flush(&mut self) {
        if self.echo_on() {
            self.echo.push(LF);
        }
        let line = core::mem::take(&mut self.pending);
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

    // --- the reader's side ---

    /// Run the output-side model of the screen a discipline's bytes travel to:
    /// the cursor the grid paints with, and the questions a written sequence
    /// asks.
    ///
    /// Returns what `write_at` should put on the wire instead of the caller's
    /// buffer: the same bytes minus the questions this line discipline answers
    /// itself, and minus the bytes of any sequence still in flight. `None` -
    /// the common case - means forward everything untouched.
    pub fn note_output(&mut self, bytes: &[u8], term: &Attached) -> Option<alloc::vec::Vec<u8>> {
        // The size the cursor is counted against is the size the terminal
        // reports, or the two answers a program can ask for disagree.
        if self.screen.rows() != term.rows || self.screen.cols() != term.cols {
            self.screen.set_size(term.rows, term.cols);
        }
        let kernel_draws = term.kernel_draws;
        // A question the discipline answers itself must not also reach the
        // wire: whatever real terminal sits beyond it would answer too, and
        // that second answer comes back as typed input - the ghost `R` a
        // fresh prompt showed, parsed and half-echoed by the shell.
        //
        // That is only true if *none* of the question travels. Whether a
        // sequence is such a question is known only when it ends, so the whole
        // of it is held here until then: a swallow measured against one
        // `write` leaves the head of a sequence that straddles two of them on
        // the wire. `bat` asks for the terminal's colours exactly that way -
        // `ESC ] 10 ; ?` in one write, its terminator in the next - so the
        // orphaned query reached the real terminal, its reply came back as
        // typed input, and every `bat` prompt ended up holding
        // `]10;rgb:abab/b2b2/bfbf`.
        // Nothing in sight of a sequence, and none carried over: the state
        // machine still has to run - the cursor it keeps is what answers a
        // later `ESC [ 6n` - but no byte needs a second home, so the
        // buffer is not allocated at all. `bat` renders by writing one colour
        // token per `write(2)`, tens of thousands of them for a minified
        // stylesheet, and this is the hot path of every one.
        let plain =
            self.out_esc == Esc::Ground && self.out_held.is_empty() && !bytes.contains(&0x1b);
        let mut out: alloc::vec::Vec<u8> = if plain {
            alloc::vec::Vec::new()
        } else {
            alloc::vec::Vec::with_capacity(bytes.len())
        };
        // The sequence in flight, carried between calls for as long as it is
        // open. Its bytes have not gone anywhere.
        let mut held = core::mem::take(&mut self.out_held);
        for &b in bytes {
            // The byte extends the sequence in flight, or opens one; anything
            // else is ordinary output.
            if !held.is_empty() || b == 0x1b {
                held.push(b);
            } else if !plain {
                out.push(b);
            }
            // What the step below decided, in the terms this loop needs.
            let mut answered = false;
            match (self.out_esc, b) {
                // An OSC/DCS string in flight. BEL ends an OSC only (a DCS
                // keeps buffering); `ESC \` ends either, handled via `Esc::One`.
                (Esc::Osc, 0x07) => {
                    if self.out_dcs {
                        if self.out_osc.len() < OSC_MAX {
                            self.out_osc.push(b);
                        }
                    } else {
                        answered = self.finish_osc(kernel_draws);
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
                // The single-character cursor controls. They travel on - whoever
                // draws the screen needs them as much as the discipline does -
                // but they move the cursor a later report answers with, so the
                // model moves with them. A sequence the model stops following is
                // a position the shell is told wrong, which is the whole bug this
                // rewrite is about.
                (Esc::Start, b'D') => {
                    self.screen.line_feed();
                    self.out_esc = Esc::Ground;
                }
                (Esc::Start, b'E') => {
                    self.screen.carriage_return();
                    self.screen.line_feed();
                    self.out_esc = Esc::Ground;
                }
                (Esc::Start, b'M') => {
                    self.screen.reverse_line_feed();
                    self.out_esc = Esc::Ground;
                }
                (Esc::Start, b'7') => {
                    self.screen.save();
                    self.out_esc = Esc::Ground;
                }
                (Esc::Start, b'8') => {
                    self.screen.restore();
                    self.out_esc = Esc::Ground;
                }
                (Esc::Start, b'c') => {
                    self.screen.reset();
                    self.out_esc = Esc::Ground;
                }
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
                    // Where the kernel draws the screen, a question this
                    // discipline answers never travels: whatever real terminal
                    // sits beyond the wire would answer it a second time, and that
                    // second answer comes back as typed input. Where something
                    // else draws it, nothing is answered and nothing is swallowed.
                    answered = self.output_csi(b, &params, kernel_draws);
                }
                (Esc::One, b'\\') if self.in_string_seq => {
                    // `ESC \` is the ST terminator of the string in flight.
                    answered = if self.out_dcs {
                        self.finish_dcs(kernel_draws)
                    } else {
                        self.finish_osc(kernel_draws)
                    };
                }
                (Esc::One, _) => {
                    self.out_esc = Esc::Ground;
                    self.in_string_seq = false;
                    self.out_dcs = false;
                }
                (Esc::Ground, LF) => {
                    // A console reads a bare line feed as a newline - home and
                    // down - which is what the renderer's grid does with it too.
                    self.screen.carriage_return();
                    self.screen.line_feed();
                }
                // Vertical tab and form feed are a newline to a screen as well;
                // `^L` is the shell's reprint, and the grid treats all three the
                // same way, so this must too.
                (Esc::Ground, 0x0b | 0x0c) => {
                    self.screen.carriage_return();
                    self.screen.line_feed();
                }
                (Esc::Ground, CR) => self.screen.carriage_return(),
                (Esc::Ground, 0x08) => self.screen.backspace(1),
                (Esc::Ground, 0x09) => self.screen.tab(),
                // A bell says nothing and moves nothing.
                (Esc::Ground, 0x07) => {}
                // The byte class the grid prints, and nothing else. Counting a
                // control byte as a cell is how a report comes back a column
                // ahead of the screen.
                (Esc::Ground, b'\x20'..=b'\x7e' | b'\xa0'..=b'\xff') => {
                    self.screen.print();
                }
                _ => {}
            }
            // The sequence ended: what it held goes on the wire, unless this
            // discipline answered it, in which case it goes nowhere.
            if self.out_esc == Esc::Ground {
                if answered {
                    held.clear();
                } else {
                    out.append(&mut held);
                }
            } else if held.len() > HELD_MAX {
                // No terminator in sight: it was not a sequence after all, and
                // holding it any longer only loses output.
                self.out_esc = Esc::Ground;
                self.in_string_seq = false;
                self.out_dcs = false;
                out.append(&mut held);
            }
        }
        self.out_held = held;
        // A plain run holds nothing: it travels as it came.
        if plain {
            return None;
        }
        if out.len() == bytes.len() && self.out_held.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// One finished output sequence: move the cursor this kernel's model of the
    /// screen keeps, and answer the question the sequence asked - if, and only if,
    /// this kernel is the terminal that owns that screen.
    ///
    /// `kernel_draws` is that decision and the only place it is made. Where it is
    /// `false` the sequence travels on and nothing is answered, because whatever
    /// sits at the other end of the wire *is* the terminal and knows where its own
    /// cursor stands. Measured on Linux: a pty pair's line discipline forwarded
    /// `ESC [ 6 n`, `ESC [ 5 n`, `ESC [ c`, `ESC [ ? 6 n`, `OSC 11 ; ?` and
    /// `DCS 1 + q` byte for byte to the master and answered none of them. Answering
    /// anyway is what cost `fish` its output here: the shell pads the dangling line
    /// out to the width it was told, and a width guessed at `80` in front of a
    /// window of any other size never reaches the margin, never wraps, and the
    /// `\r` and `ESC [ K` that follow erase the line the command just printed.
    ///
    /// `true` says the discipline answered it itself, which is what tells the
    /// caller to keep the bytes off the wire. Which sequences are questions and
    /// which are answered is one decision read from one place: it used to be two,
    /// the swallow measured off the raw parameter *bytes* and the answer off the
    /// parsed ones, so `ESC [ ? 6 n` - a cursor question the DEC-private marker
    /// makes no difference to - was forwarded unanswered while `ESC [ 6 n` was
    /// answered. A question the console does not answer is a caller that waits, so
    /// both markers answer here, as Linux's `csi_DEC` and `csi_ECMA` both do.
    ///
    /// Every arm that moves the cursor calls [`Cursor`], and
    /// [`crate::term::Grid`] calls the same ones from the same bytes, because
    /// a report that describes a cursor the screen does not have is the exact
    /// failure this function used to have.
    fn output_csi(&mut self, final_byte: u8, params: &[u8], kernel_draws: bool) -> bool {
        let (first, second) = Self::params(params);
        let private = params
            .iter()
            .any(|b| matches!(b, b'?' | b'<' | b'=' | b'>'));
        match final_byte {
            b'A' => self.screen.up(first),
            b'B' | b'e' => self.screen.down(first),
            b'C' | b'a' => self.screen.right(first),
            b'D' => self.screen.left(first),
            b'E' => {
                self.screen.carriage_return();
                self.screen.down(first);
            }
            b'F' => {
                self.screen.carriage_return();
                self.screen.up(first);
            }
            b'G' | b'`' => self.screen.set_column(first),
            b'd' => self.screen.set_line(first),
            b'H' | b'f' => self.screen.move_to(first, second),
            // Erasing moves nothing. `CSI 2 J` clears the screen and leaves the
            // cursor where it was - which is what the grid does, so it is what a
            // report has to say. `clear` and `^L` send the `CSI H` that homes
            // explicitly, and this used to home on the erase itself.
            b'J' | b'K' => {}
            // `DECSTBM`: the region the cursor moves inside, then the cursor to
            // the region's top left. The grid does both; a report that knows only
            // one of them is a position the screen disagrees with.
            b'r' => {
                let (top, bottom) = Self::two_params(params, self.screen.rows());
                if self.screen.set_region(top, bottom) {
                    let (t, _) = self.screen.region();
                    self.screen.move_to(t + 1, 1);
                }
            }
            b's' => self.screen.save(),
            b'u' => self.screen.restore(),
            // The alternate screen. `less`, `vi` and a pager's `-e` draw over a
            // cleared window and, on leaving, put the screen back; the grid parks
            // its cells and this model parks the cursor with them, or the shell
            // that resumes is told a position the screen gave back pages ago.
            b'h' | b'l' if params.contains(&b'?') => {
                let on = final_byte == b'h';
                let (modes, count) = Self::parse_params(params);
                for &mode in modes.iter().take(count) {
                    match mode {
                        47 | 1047 | 1049 => {
                            // Idempotent in both directions, like the grid: a
                            // second enter must not lose the parked cursor, and an
                            // exit nobody parked for moves nothing.
                            if on && self.alt_cursor.is_none() {
                                self.alt_cursor = Some(self.screen.take_alt());
                            } else if !on && let Some(saved) = self.alt_cursor.take() {
                                self.screen.resume_alt(saved);
                            }
                        }
                        // Cursor visibility, the key modes, wrap mode: all drawn
                        // with, none of them a cursor move.
                        _ => {}
                    }
                }
            }
            b'n' if kernel_draws => {
                // A question. `5n` asks whether the terminal is there; `6n`
                // asks where the cursor is, and a shell waits for the answer
                // before it reads another key - so the answer goes back the
                // way a terminal's would: as input. The DEC-private marker
                // changes nothing: Linux answers `n` from `csi_DEC` exactly as
                // from `csi_ECMA`.
                let asked = Self::first_query_param(params);
                if asked == 5 {
                    self.answer(b"\x1b[0n");
                    return true;
                } else if asked == 6 {
                    let (row, col) = self.screen.pos();
                    self.answer(b"\x1b[");
                    let row = Self::decimal(row + 1);
                    self.answer(&row);
                    self.answer(b";");
                    let col = Self::decimal(col + 1);
                    self.answer(&col);
                    self.answer(b"R");
                    return true;
                }
            }
            // Device Attributes. The console answers exactly one shape of this:
            // `csi c`/`csi 0 c`, and `respond_ID()`'s answer is the VT102 ID
            // `\e[?6c` - which is the whole of its Device-Attributes vocabulary,
            // no `CSI > c` (secondary DA) and no seven-parameter feature list.
            //
            // `CSI ? Ps c` is not a question at all: it is `csi_DEC`'s
            // *cursor-type set*, and the `linux` terminfo the shells and editors
            // here read builds its cursor capabilities out of it - `civis` is
            // `\e[?25l\e[?1c`, `cnorm` is `\e[?25h\e[?0c`, `cvvis` is
            // `\e[?25h\e[?8c`. vim writes `civis` and `cnorm` on every command
            // line it paints and again as it leaves. Answering those sets is
            // what left `9c9c` sitting on the prompt after vim exited: two
            // replies no one had asked for, queued as input, whose tails the
            // shell then read as typed text and tried to run.
            b'c' if kernel_draws && !private && Self::first_query_param(params) == 0 => {
                self.answer(b"\x1b[?6c");
                return true;
            }
            // Everything else - colours, the modes, what this kernel does not
            // draw - costs the cursor nothing and is answered by nobody here.
            _ => {}
        }
        false
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

    /// A CSI's parameters as numbers, up to [`PARAMS_MAX`] of them, and how many
    /// were read. A private marker (`?`, `>`, `<`, `=`) and an intermediate
    /// (`0x20..=0x2f`) are not parameters; an unwritten one is zero, which is what
    /// makes `ESC [ ; 5 r` a region whose top is the default.
    fn parse_params(bytes: &[u8]) -> ([usize; PARAMS_MAX], usize) {
        let mut out = [0usize; PARAMS_MAX];
        let mut count = 0usize;
        let mut value = 0usize;
        let mut seen = false;
        for &b in bytes {
            match b {
                b'?' | b'<' | b'=' | b'>' | 0x20..=0x2f if !seen => continue,
                b'0'..=b'9' => {
                    value = value * 10 + (b - b'0') as usize;
                    seen = true;
                }
                b';' | b':' => {
                    if count < PARAMS_MAX {
                        out[count] = value;
                        count += 1;
                    }
                    value = 0;
                    seen = false;
                }
                _ => break,
            }
        }
        if seen && count < PARAMS_MAX {
            out[count] = value;
            count += 1;
        } else if count == 0 {
            // No digits at all: one parameter, and it is the default zero.
            out[0] = 0;
            count = 1;
        }
        (out, count)
    }

    /// A CSI's first two parameters, with the second defaulting to `bottom` when
    /// the sequence leaves it out - which is what `DECSTBM` wants (`ESC [ 3 r` is
    /// "from line 3 to the bottom of the screen") and not what a cursor move wants.
    fn two_params(bytes: &[u8], bottom: usize) -> (usize, usize) {
        let (p, n) = Self::parse_params(bytes);
        let top = if n > 0 && p[0] != 0 { p[0] } else { 1 };
        let bot = if n > 1 && p[1] != 0 { p[1] } else { bottom };
        (top, bot)
    }

    /// The value of a CSI's first parameter, read the way Linux's console reads
    /// a question: the private marker (`?`, `>`, `<`, `=`) and any intermediate
    /// are not parameters, and an unwritten parameter is `0` - so `ESC [ c`,
    /// `ESC [ 0 c` and `ESC [ ? 6 n` ask for `c`-question 0, 0 and a cursor
    /// report. [`Self::params`] cannot serve here: it takes the marker for the
    /// first parameter and defaults an omitted one to `1`, which is right for a
    /// cursor movement and wrong for a question.
    fn first_query_param(bytes: &[u8]) -> usize {
        let mut value = 0usize;
        for b in bytes {
            match b {
                b'?' | b'<' | b'=' | b'>' => continue,
                0x20..=0x2f => continue,
                b'0'..=b'9' => value = value * 10 + (b - b'0') as usize,
                _ => break,
            }
        }
        value
    }

    /// Reply the way a terminal replies: on the input side.
    fn answer(&mut self, bytes: &[u8]) {
        self.answer.extend(bytes.iter().copied());
    }

    /// An OSC string ended (on BEL or ST). If its body was a colour *query* and
    /// this kernel is the terminal, answer it - and report that it was answered, so
    /// the caller keeps it off the wire and the real terminal cannot answer a second
    /// time. Where a real terminal is downstream the query is neither answered nor
    /// swallowed: it is that terminal's question to answer.
    fn finish_osc(&mut self, kernel_draws: bool) -> bool {
        self.out_esc = Esc::Ground;
        self.in_string_seq = false;
        self.out_dcs = false;
        let payload = core::mem::take(&mut self.out_osc);
        if !kernel_draws {
            return false;
        }
        let (index, rgb) = match osc_color_query(&payload) {
            Some(v) => v,
            None => return false,
        };
        // `\e] <index> ; <rgb> \e\` - the reply a terminal gives a colour query.
        self.answer(b"\x1b]");
        self.answer(&Self::decimal(index));
        self.answer(b";");
        self.answer(rgb.as_bytes());
        self.answer(b"\x1b\\");
        true
    }

    /// A DCS string ended (on ST). A capability query (`ESC P 1 + q <hex>`,
    /// XTGETTCap - what fish/neovim/tmux use to ask a terminal its dynamic
    /// capabilities) is answered here with an empty reply, and kept off the wire,
    /// only when this kernel is the terminal. Any other DCS, and every DCS where a
    /// real terminal is downstream, is left to be forwarded.
    fn finish_dcs(&mut self, kernel_draws: bool) -> bool {
        self.out_esc = Esc::Ground;
        self.in_string_seq = false;
        self.out_dcs = false;
        let body = core::mem::take(&mut self.out_osc);
        if !kernel_draws {
            return false;
        }
        if body.starts_with(b"1+q") {
            self.answer(b"\x1bP1+r\x1b\\");
            return true;
        }
        false
    }

    pub fn termios(&self) -> Termios {
        self.tio
    }

    pub fn set_termios(&mut self, tio: Termios) {
        let was_canonical = self.tio.lflag & ICANON != 0;
        self.tio = tio;
        // A held literal belonged to the settings that were.
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

    /// `VMIN`, in the only sense Linux gives it in non-canonical mode: a
    /// *minimum* number of bytes a read wants, where 0 means "whatever is
    /// there, and nothing is an answer".
    fn vmin(&self) -> u8 {
        self.tio.cc[VMIN]
    }

    /// `VTIME`, in deciseconds. 0 means the read has no clock at all.
    fn vtime(&self) -> u8 {
        self.tio.cc[VTIME]
    }

    /// Whether the queue holds the bytes a read considers ready.
    ///
    /// An answer and an end of input come first: a shell that asked where the
    /// cursor is must not have its reply sit behind a `VMIN` it never set, and
    /// an EOF is a complete answer in itself.
    ///
    /// Non-canonical mode then reads `VMIN` as the measured minimum - which
    /// means `VMIN == 0` makes *any* byte ready. The `.max(1)` that used to
    /// stand here was argued from "a read returning nothing looks like
    /// end-of-file", and that is exactly the case Linux answers without
    /// blocking; `VMIN == 0` is how busybox `vi` and `vim` disambiguate a bare
    /// `ESC`, so forbidding it froze them with the escape key held down. The
    /// zero-byte answer is not ambiguous here, because an end of input travels
    /// as a completed read and this state says only "wait" or "answer 0 now" -
    /// see [`read_wait`](Self::read_wait).
    pub fn has_ready(&self) -> bool {
        if !self.answer.is_empty() || self.eof {
            return true;
        }
        if self.tio.lflag & ICANON != 0 {
            !self.out.is_empty()
        } else {
            !self.out.is_empty() && self.out.len() >= self.vmin() as usize
        }
    }

    /// The wait a blocking read owes its caller, given the tick the read
    /// started at and how many ticks a decisecond is.
    ///
    /// Every branch of this table is a measurement on Linux, not a reading of
    /// the man page, which is ambiguous about what `VTIME` times. The shape it
    /// found is that `VTIME` is an **inter-byte** clock in non-canonical mode:
    ///
    /// | `VMIN`/`VTIME` | queued | measured |
    /// |---|---|---|
    /// | any | an answer, or an end of input | read it now |
    /// | any, canonical | a partial line | blocks (4 s measured): canonical mode consults neither `VMIN` nor `VTIME`, only a line terminator or EOF |
    /// | `0`/`0` | nothing | **0 bytes immediately** - the poll answer |
    /// | `>0`/`0` | fewer than `VMIN` | blocks indefinitely, however many are queued |
    /// | `0`/`>0` | nothing | 0 bytes one `VTIME` after the read *began* (503 ms at `VTIME=5`) |
    /// | `0`/`>0` | any | those bytes **as they arrive** (100 ms at `VTIME=2`, not 300) |
    /// | `>0`/`>0` | fewer than `VMIN` | what is queued, one `VTIME` after the **last** byte landed |
    /// | `>0`/`>0` | at least `VMIN` | everything queued, immediately: 5 bytes at once for `VMIN=3` |
    ///
    /// `entry` anchors only the `VMIN == 0` clock, and `last_byte` the other;
    /// a caller that has no clock hands 0 for both and gets a wait that can
    /// only end when input arrives, which is the host-visible half of the rule.
    ///
    /// `nonblocking` is the descriptor's `O_NONBLOCK`, and it is a fourth input
    /// to the same decision rather than a rule of its own: a read that asked not
    /// to wait takes what is there and otherwise hears `EAGAIN`. Linux's own
    /// table, measured on a pty slave (`O_NONBLOCK` set, nothing else done):
    ///
    /// | `VMIN`/`VTIME` | queued | answered |
    /// |---|---|---|
    /// | `0`/`0` | nothing | 0 bytes - the poll, which never waited |
    /// | `0`/`0` | 3 bytes | those 3 bytes |
    /// | `>0`/`0` | nothing | `EAGAIN` |
    /// | `0`/`>0` | nothing | `EAGAIN` at 0 ms - the clock is not consulted |
    /// | `>0`/`>0` | nothing | `EAGAIN` |
    /// | `2`/`5` | 1 byte | that 1 byte, immediately: the minimum is a floor on a *wait*, not on a read that is not waiting |
    /// | `5`/`0` | 2 bytes | those 2 bytes, for the same reason |
    /// | canonical | a partial line | `EAGAIN` |
    /// | canonical | a complete line | the line |
    pub fn read_wait(&self, entry: u64, ticks_per_ds: u64, nonblocking: bool) -> ReadWait {
        if self.has_ready() {
            return ReadWait::Ready;
        }
        if nonblocking {
            return self.nonblocking_wait();
        }
        // Canonical: a read waits for a line, and nothing else ends the wait.
        if self.tio.lflag & ICANON != 0 {
            return ReadWait::UntilInput;
        }
        let (vmin, vtime) = (self.vmin(), self.vtime());
        if vmin == 0 && vtime == 0 {
            // The poll: an empty queue is the answer, and it is 0 bytes rather
            // than an end of input - which is why the read reports it itself
            // instead of going through `take`, whose 0-byte slice *is* an EOF.
            return ReadWait::Empty;
        }
        if vtime == 0 {
            return ReadWait::UntilInput;
        }
        let span = (vtime as u64).saturating_mul(ticks_per_ds);
        if vmin == 0 {
            // Bytes that land are taken as they arrive, so the clock is only
            // there to answer an empty queue - and it runs from the read's own
            // start, not from a byte that never came.
            return ReadWait::UntilTick(entry.saturating_add(span));
        }
        // `VMIN > 0`: with nothing queued there is no clock to run - the read
        // waits for a first byte with unlimited patience - and once bytes are
        // there the clock is the gap since the last one arrived.
        if self.out.is_empty() {
            return ReadWait::UntilInput;
        }
        ReadWait::UntilTick(self.last_byte.saturating_add(span))
    }

    /// [`Self::read_wait`] for a descriptor that set `O_NONBLOCK`: the two
    /// answers that are not a wait.
    ///
    /// What is already takeable is taken - a minimum is a floor on a wait, and
    /// this read is not waiting (measured: `VMIN=5/VTIME=0` with two bytes
    /// queued answered both, immediately). Canonical mode is the one place the
    /// queue and the answer differ: its partial line lives in the editing
    /// buffer, not in `out`, so `out` empty there means `EAGAIN` and not "hand
    /// over half a line". And `VMIN == 0`/`VTIME == 0` keeps its zero-byte
    /// answer, because that read was never going to wait - it is the poll.
    fn nonblocking_wait(&self) -> ReadWait {
        if self.tio.lflag & ICANON == 0 && !self.out.is_empty() {
            return ReadWait::Ready;
        }
        if self.tio.lflag & ICANON == 0 && self.vmin() == 0 && self.vtime() == 0 {
            return ReadWait::Empty;
        }
        ReadWait::WouldBlock
    }

    /// Whether a byte arriving must wake a reader that is still not satisfied,
    /// because it starts or restarts that reader's `VTIME` clock.
    ///
    /// Without this a `VMIN=2/VTIME=5` read sleeps through the first byte - the
    /// byte makes no read decision on its own - and then wakes only when its
    /// own deadline passes, which by then was computed from a queue this byte
    /// just changed. The extra wake is what re-anchors the clock, and it is how
    /// the measured "a late byte is delivered 500 ms after it arrives" happens.
    pub fn byte_restarts_clock(&self) -> bool {
        self.tio.lflag & ICANON == 0 && self.vtime() > 0
    }

    /// Record the tick an input byte landed at, for [`read_wait`](Self::read_wait).
    pub fn note_input_tick(&mut self, tick: u64) {
        self.last_byte = tick;
        self.input_count = self.input_count.wrapping_add(1);
    }

    /// The running count of input bytes landed. A clock-restarting read snapshots
    /// this before parking and wakes when it changes; see `input_count` on the
    /// struct.
    pub fn input_count(&self) -> u64 {
        self.input_count
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
/// Who is asking a console a `'V'`-family question.
///
/// The family turns on three facts that live *above* the VFS - which task is
/// asking (the foreground question every acting request starts with), whether it
/// is root (the superuser may act on a console that is not its own), and which
/// thread group a `VT_SETMODE` promise belongs to (Linux's `vc->vt_pid`) - so the
/// syscall layer, which may name `sched`, answers all three here rather than
/// letting the console device go looking for the caller's identity. It is the
/// same hand-off [`Tty::kd_ioctl`]'s `owns_card` is, and it is what keeps the
/// console out of the scheduler.
#[cfg(target_os = "none")]
#[derive(Clone, Copy)]
pub struct VtCaller {
    /// The task asking, for [`Tty::is_foreground`].
    pub task: TaskId,
    /// Whether it is root - answered by the caller layer, because this one
    /// cannot ask.
    pub root: bool,
    /// The thread group the promise goes to.
    pub tgid: u32,
}

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
    /// The node's mode and owner: `chmod`/`chown` on a real terminal - and
    /// udev's cold-plug rules for `0620 root:tty` - land here and are read
    /// back by `stat` and by the permission checks. Born `0666 root:root`,
    /// the number `stat` already gave these nodes before any of this existed
    /// (see `devfs::NodePerms`).
    perms: super::devfs::NodePerms,
    /// The window a program is told it has, in `(rows, columns)`, for a
    /// terminal whose size the kernel cannot know: what `TIOCSWINSZ` last set,
    /// and **`0x0`** until then - which is the one answer Linux gives for a
    /// terminal nobody has sized, and the only honest one: see
    /// [`Tty::window_size`]. A virtual console stores it too but is answered
    /// from its screen instead.
    winsize: Mutex<RefCell<(u16, u16)>>,
    /// `O_NONBLOCK` on the description that opened this terminal - the bit the
    /// read consults before it parks. Mirrored from the description the way a
    /// pipe's and an eventfd's is (`sync_nonblocking`), because the flag lives
    /// on the open file description rather than on the terminal.
    ///
    /// One terminal, one bit, and that is the same approximation the other
    /// node-level flags make: two descriptions of one pty slave that disagree
    /// about `O_NONBLOCK` are decided here by whichever set it last, where
    /// Linux would keep the two apart. What the bit has to get right is the
    /// common shape - a reader that opened (or `fcntl`ed) its terminal
    /// nonblocking so its event loop never parks - and that is one description.
    nonblocking: core::sync::atomic::AtomicBool,
}

impl Tty {
    pub const fn new(dev_id: (u32, u32), output: TtyOutput) -> Tty {
        Tty {
            ld: Mutex::new(RefCell::new(LineDiscipline::new())),
            waiters: Mutex::new(RefCell::new(Vec::new())),
            foreground: Mutex::new(RefCell::new(None)),
            output: Mutex::new(RefCell::new(output)),
            dev_id,
            perms: super::devfs::NodePerms::new(0o666, 0, 0),
            winsize: Mutex::new(RefCell::new((0, 0))),
            nonblocking: core::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Mirror the opening description's `O_NONBLOCK` into the terminal, where
    /// the read that would park consults it.
    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking
            .store(on, core::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the description asked not to wait - see [`Tty::nonblocking`].
    pub fn is_nonblocking(&self) -> bool {
        self.nonblocking.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The window a program is told it has, in `(rows, columns)` - what
    /// `TIOCGWINSZ` answers.
    ///
    /// A *virtual console* answers the size of the screen it is painted on: the
    /// framebuffer console is that console's terminal emulator, so the grid it
    /// draws into *is* the window, and it is computed from the scanout at
    /// `CELL_W`x`CELL_H` (see `drivers/video/render.rs`). Answering anything
    /// else is how a full-screen program came to lay itself out in the corner
    /// of the screen: this used to be a constant 25x80, so on a 1024x768
    /// display - a 128x48 grid - `vim` drew 80 of its 128 columns and 25 of its
    /// 48 rows and left the rest blank.
    ///
    /// Anything else - the serial wire, a `/dev/ptmx` pair - answers what
    /// `TIOCSWINSZ` last set, and `0x0` until then.
    ///
    /// `0x0` is not a placeholder this kernel picked, it is the measured Linux
    /// answer, and the difference from a guessed `25x80` is the whole point: a
    /// pty nobody has sized reports zero rows and zero columns there (Emacs'
    /// own `getwinsize` printing `0 columns 0 rows` is the bug report that
    /// pinned it, `debbugs.gnu.org/24608`), and a program answered `0` *knows*
    /// it was told nothing, so it falls back in the order its own
    /// documentation names - `LINES`/`COLUMNS`, then the terminfo `li` and
    /// `co`, then `80x24` - or asks the terminal itself where its cursor is
    /// and works out the size from that (`vim` does both).
    ///
    /// A guessed `25x80` takes those fallbacks away from a program that would
    /// have used them and hands it a number the window it is drawing into does
    /// not have, which is how a `vi` over the serial wire came to draw 25 of
    /// the host's 40 rows and leave the rest of the window holding a page that
    /// had already gone. Telling the truth about not knowing is worth more than
    /// being nearly right.
    pub fn window_size(&self) -> (u16, u16) {
        // Asked rather than cached, so a console whose screen changed under it
        // - a different scanout, a console the mux moved - answers correctly
        // with no bookkeeping to keep in step.
        if let Some(vc) = self.vc_number()
            && let Some((rows, cols)) =
                crate::term::with_screen(vc as usize, |g| (g.rows(), g.cols()))
        {
            return (rows as u16, cols as u16);
        }
        critical_section::with(|cs| *self.winsize.borrow(cs).borrow())
    }

    /// Record a window size - `TIOCSWINSZ` - and report whether it *changed*,
    /// which is the caller's cue to raise `SIGWINCH`.
    ///
    /// A virtual console stores it and does not answer it: the screen is that
    /// console's real size and [`window_size`](Self::window_size) prefers it, so
    /// a `stty rows` on one is accepted and has no effect rather than answering
    /// a number that disagrees with what is being painted. On a terminal where
    /// nothing else knows the size, this is the size.
    pub fn set_window_size(&self, rows: u16, cols: u16) -> bool {
        critical_section::with(|cs| {
            let mut size = self.winsize.borrow(cs).borrow_mut();
            let changed = *size != (rows, cols);
            *size = (rows, cols);
            changed
        })
    }

    /// Whose screen this terminal's bytes land on, and how big it is - the two
    /// facts the line discipline needs and cannot work out for itself.
    ///
    /// Read per write rather than cached, for the same reason
    /// [`window_size`](Self::window_size) is: a console whose screen changed under
    /// it - a scanout resized, a grid the render task has only just made - has to
    /// be answered from what is true now, or the cursor report and the `TIOCGWINSZ`
    /// report split apart, which is the failure this exists to prevent.
    pub fn attached(&self) -> Attached {
        let (rows, cols) = self.window_size();
        Attached {
            kernel_draws: self.kernel_draws(),
            rows: rows as usize,
            cols: cols as usize,
        }
    }

    /// Whether *this kernel* is the terminal emulator for this terminal's output:
    /// whether the bytes it is asked to draw end up in a screen the kernel owns.
    ///
    /// This is the question a `ESC [ 6 n` turns on, and it is not the question
    /// "is this a virtual console". The six consoles are pty pairs like any
    /// `/dev/pts`, and what makes them different is only who reads the master:
    ///
    /// * the console multiplexer, which hands the bytes to a [`crate::term`]
    ///   grid - the kernel draws the screen, so the kernel answers, because there
    ///   is no one else who knows where that cursor is, and a shell that asks and
    ///   is refused waits forever;
    /// * the serial wire, when no grid exists (`--mode terminal`) - the person's
    ///   own terminal window is the emulator, it has the cursor, and Linux's
    ///   measured answer for a pty pair is that the line discipline forwards the
    ///   query untouched. This kernel answering instead is what put a guessed
    ///   `80` in front of a window that is not `80` wide, and let `fish` erase the
    ///   line its own command had just printed;
    /// * a process holding the master (`/dev/pts` under a terminal program) - the
    ///   same forwarding, and the same measurement.
    ///
    /// `Discard` answers `true` on being asked nothing else: a terminal with no
    /// wire behind it has no other responder, and silence there is a hang.
    fn kernel_draws(&self) -> bool {
        let output = critical_section::with(|cs| *self.output.borrow(cs).borrow());
        match output {
            TtyOutput::Discard => true,
            // The kernel console's own voice: `console::write_bytes` paints the
            // active grid for as long as a grid exists, and the wire alone when it
            // does not.
            TtyOutput::Serial => kernel_console_has_grids(),
            TtyOutput::Pty(_) => match self.vc_number() {
                Some(vc) => crate::term::has_screen(vc as usize),
                None => false,
            },
        }
    }

    /// Tell the foreground group the window changed - `SIGWINCH`, the one signal
    /// a full-screen program redraws on.
    ///
    /// The same seam an interrupt takes, and for the same reason: a pipeline is
    /// one job and every process in it has the same window, so a resize aimed at
    /// the job is aimed at all of it. A program that does not catch it carries
    /// on, which is what the default action says.
    #[cfg(target_os = "none")]
    pub fn signal_window_change(&self) {
        if let Some(pgid) = self.foreground() {
            crate::sched::send_signal_group(pgid, SIGWINCH);
        }
    }

    /// Which of the six virtual consoles this terminal *is*, 1-based, or `None`
    /// for a terminal that is not one: a `/dev/pts` slave, a pty master.
    ///
    /// The console's modes - `KDSETMODE`'s video mode and `KDSKBMODE`'s keyboard
    /// mode - are kept per console, so an ioctl on a tty has to say which
    /// console it means before it can mean anything. A slave's device number is
    /// its console number already (`(4, n)` is `/dev/tty<n>`), and `/dev/tty0` -
    /// "the console I am on now", which is what a seat manager opens - names the
    /// active one rather than a console of its own.
    pub fn vc_number(&self) -> Option<u32> {
        match self.dev_id {
            (4, n) if (1..=VC_COUNT).contains(&n) => Some(n),
            // `(4, 0)` is `/dev/tty0`, "the console I am on now", and `(4, 64)`
            // is the boot console's own tty - which is what a seat manager holds
            // before it has asked for a console by number. Both mean the active
            // one. A host build has no console mux running to be active.
            #[cfg(target_os = "none")]
            (4, 0) | (4, 64) => Some(crate::term::vc::active()),
            _ => None,
        }
    }

    /// Send bytes to this terminal's wire. Called for echo, for `write(2)`, and
    /// for the answers the line discipline gives a cursor query - so all three
    /// land on the same wire the terminal is attached to.
    ///
    /// It is also where output post-processing happens, and for the same
    /// reason: this is the one place every byte the terminal *says* passes
    /// through, whichever of the three produced it. `OPOST|ONLCR` turn a line
    /// feed into carriage-return-line feed, which is the difference between a
    /// terminal and a printer - see [`Termios::console_default`] for why this
    /// kernel used not to.
    ///
    /// Answers how many of `bytes` the wire took, which is all of them except
    /// on the one wire that can fill: a pty's master buffer, where a full
    /// buffer ends a writer's wait - by `O_NONBLOCK` or by a signal - and the
    /// honest answer is then the partial count the caller retries from. The
    /// two errors a full buffer gives with nothing taken (`EAGAIN`, `EINTR`)
    /// travel out as errors, since a write of zero bytes has no other meaning.
    fn write_out(&self, bytes: &[u8], wire: Wire) -> VfsResult<usize> {
        let input = bytes;
        let input_len = input.len();
        let output = critical_section::with(|cs| *self.output.borrow(cs).borrow());
        let oflag = critical_section::with(|cs| self.ld.borrow(cs).borrow().termios().oflag);
        // The rest of the output flags - `OCRNL`, `ONOCR`, `ONLRET`, `OLCUC`,
        // tab expansion - are off in every default and in what a shell sets,
        // and each is a rule about a printer or a different line discipline.
        // Honouring this one is what a screen needs; the others would be
        // inventions until something asked for them.
        let processed: Cow<[u8]> =
            if oflag & OPOST != 0 && oflag & ONLCR != 0 && bytes.contains(&LF) {
                Cow::Owned(newline_expanded(bytes))
            } else {
                Cow::Borrowed(bytes)
            };
        let bytes: &[u8] = &processed;
        #[cfg(target_os = "none")]
        match output {
            TtyOutput::Serial => {
                crate::console::write_bytes(bytes);
                Ok(processed.len().min(input_len))
            }
            // The master's buffer is the one wire here that can fill: what it
            // hands back is how much of the *processed* bytes got in, and the
            // caller is owed a count in its own units.
            TtyOutput::Pty(number) => match wire {
                Wire::Task => {
                    let taken =
                        crate::vfs::pty::master_input(number, bytes, self.is_nonblocking())?;
                    if taken == processed.len() {
                        Ok(input_len)
                    } else {
                        // Short by the wire's measure, so the count is re-derived
                        // in the caller's: whole input bytes only. A boundary
                        // that fell between an expanded `CR LF` therefore
                        // re-offers that one input byte, whose carriage return is
                        // written twice - which renders identically, where the
                        // alternative (counting the byte) would lose the line
                        // feed. `PLAN.md` §2.5 records it.
                        Ok(input_prefix(input, taken))
                    }
                }
                Wire::Interrupt => {
                    crate::vfs::pty::master_input_interrupt(number, bytes);
                    Ok(input_len)
                }
            },
            TtyOutput::Discard => Ok(input_len),
        }
        // On the host there is no console to write to, but the pty wire is
        // plain memory - the pair tests drive it exactly as the machine does.
        // Nothing there can fill, so the context `wire` names has no second
        // answer to give.
        #[cfg(not(target_os = "none"))]
        {
            let _ = wire;
            if let TtyOutput::Pty(number) = output {
                crate::vfs::pty::master_input(number, bytes, false);
            }
            Ok(input_len)
        }
    }

    /// Feed one byte from the input side, show its echo, and act on what the
    /// byte asked for: a completed line wakes the readers, and an interrupt
    /// is delivered to whoever is reading. Runs with interrupts off for the
    /// length of the edit, which is nothing next to the byte's arrival rate.
    pub fn receive_byte(&self, byte: u8) {
        let attached = self.attached();
        let (echo, action, waiters, foreground) = critical_section::with(|cs| {
            let mut ld = self.ld.borrow(cs).borrow_mut();
            let action = ld.receive(byte);
            // Stamp the arrival before deciding who to wake: a `VMIN > 0` read
            // with a `VTIME` set has its clock anchored here, and only the
            // reader itself can re-read it - so a byte that does not complete a
            // read decision still has to wake it. See
            // [`LineDiscipline::byte_restarts_clock`].
            #[cfg(target_os = "none")]
            ld.note_input_tick(crate::hal::ticks());
            let echo = ld.take_echo();
            // Echo is output like any other: the cursor moves for the characters
            // the console shows, so the model of the screen has to follow it. A
            // discipline that only counted what a program wrote would report a
            // cursor the printed line had already left behind - and the answer is
            // the bytes a *later* line is replayed from, so the forward decision
            // the discipline makes here is thrown away on purpose.
            if !echo.is_empty() {
                let _ = ld.note_output(&echo, &attached);
            }
            // Two different wakes, and only one of them may take the list.
            // A completed read has bytes to hand over, so every waiter is
            // woken and removed - a reader re-arms on each pass of
            // `read_blocking`, which is what makes the removal safe. A clock
            // restart has nothing to give anyone: it only tells a waiting read
            // that the deadline it computed is now stale. Taking the list for
            // *that* would silently unsubscribe the waiters that do not re-arm
            // themselves - `poll`, `select` and above all `epoll`, which
            // subscribes once per wait - and an epoll subscriber dropped from
            // the list never hears about the next line.
            let waiters = if ld.has_ready() {
                core::mem::take(&mut *self.waiters.borrow(cs).borrow_mut())
            } else if ld.byte_restarts_clock() {
                self.waiters.borrow(cs).borrow().clone()
            } else {
                Vec::new()
            };
            let foreground = *self.foreground.borrow(cs).borrow();
            (echo, action, waiters, foreground)
        });
        // The echo is the discipline's own output, and it runs here - in the
        // byte's own context, often an interrupt - so there is no wait in it to
        // interrupt and no partial answer to report: `Wire::Interrupt` is the
        // push that cannot park.
        let _ = self.write_out(&echo, Wire::Interrupt);
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
            //
            // The quit and the suspend take the same route, and for the same
            // reason: Ctrl-\ and Ctrl-Z are aimed at the job as much as Ctrl-C
            // is. `SIGTSTP`'s destination is the one that matters most, because
            // a job the keyboard stopped is a job the shell has to be told
            // about: stopping only the reader would leave the rest of the
            // pipeline running behind a stopped `wait`, which is not a job
            // state anything can name.
            if let Some(pgid) = foreground {
                match action {
                    Action::Interrupt => {
                        crate::sched::send_signal_group(pgid, SIGINT);
                    }
                    Action::Quit => {
                        crate::sched::send_signal_group(pgid, SIGQUIT);
                    }
                    Action::Suspend => {
                        crate::sched::send_signal_group(pgid, SIGTSTP);
                    }
                    Action::None => {}
                }
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

    /// Serve the console's `'K'`-typed (keyboard/display-mode) ioctls. The ABI
    /// layer resolved the descriptor to this tty, read its console number, and
    /// asked the card driver whether the caller holds the screen — that last
    /// question stays in the syscall layer because the console (this vfs module)
    /// must not reach up into `drivers`; `owns_card` is handed down. Everything
    /// else — which mode a number selects, what the console stores, and which
    /// requests this device declines — is the console's own to answer, so it
    /// lives here on the device rather than in the syscall switchboard (V-2).
    /// This is `Tty`'s `KDSETMODE`/`KDGETMODE`/`KDSKBMODE`/`KDGKBMODE`/
    /// `KDGKBTYPE` serving, byte-for-byte the answers the switchboard gave, and
    /// V-4 will carry it when the console moves out of the VFS into a driver.
    ///
    /// Both machines since `PLAN.md` §13.16. Every arm but `KDGKBTYPE` reaches
    /// into `vfs::vcmux`, which used to be x86_64's because it named
    /// `drivers::{video::render,drm}` itself; it now asks `crate::console`'s
    /// `display_*` seams, so the family is served wherever `target_os` is `none`
    /// and the only thing a machine with no card changes is the answer to "do you
    /// hold the screen" - `owns_card`, which is `false` there rather than absent.
    #[cfg(target_os = "none")]
    pub fn kd_ioctl(
        &self,
        vc: u32,
        request: u32,
        buf: usize,
        from_user: bool,
        owns_card: bool,
    ) -> isize {
        use crate::err;
        use crate::uaccess::buffer_ok;
        let nr = request & 0xff;
        // The caller may change the keyboard mode if it is this console's own
        // foreground job or root — the video mode additionally accepts the card's
        // holder (`owns_card`, answered by the caller layer).
        let owns_it =
            self.is_foreground(crate::sched::current()) || crate::sched::current_is_root();
        // KDSETMODE/KDSKBMODE are `_IO`: the int argument is the plain value the
        // syscall layer handed over where `buf` is for these requests.
        let arg = buf as i32;
        match nr {
            // KDSETMODE -> int argument: the console's video mode.
            0x3a => {
                if !(owns_card || owns_it) {
                    return err::EPERM;
                }
                let mode = match arg as u32 {
                    KD_TEXT | KD_GRAPHICS => arg as u32,
                    // The two obsolete text modes, folded to the one they meant.
                    2 | 3 => KD_TEXT,
                    _ => return err::EINVAL,
                };
                if !crate::vfs::vcmux::set_kd_mode(vc, mode) {
                    return err::EINVAL;
                }
                0
            }
            // KDGETMODE <- int: what that console's screen is doing.
            0x3b => {
                if !buffer_ok(buf, 4, true, from_user) {
                    return err::EFAULT;
                }
                unsafe { (buf as *mut i32).write_unaligned(crate::vfs::vcmux::kd_mode(vc) as i32) };
                0
            }
            // KDSKBMODE -> int: the keyboard's translation mode for this console.
            // The keyboard belongs to the console's own session, so unlike the
            // video mode this is not something the card's holder gets to say —
            // which is exactly why `K_OFF` must never be the only way to stop a
            // console reading the machine's keys.
            0x45 => {
                if !owns_it {
                    return err::EPERM;
                }
                if !matches!(
                    arg as u32,
                    K_RAW | K_XLATE | K_MEDIUMRAW | K_UNICODE | K_OFF
                ) {
                    return err::EINVAL;
                }
                if !crate::vfs::vcmux::set_kb_mode(vc, arg as u32) {
                    return err::EINVAL;
                }
                0
            }
            // KDGKBMODE <- int.
            0x44 => {
                if !buffer_ok(buf, 4, true, from_user) {
                    return err::EFAULT;
                }
                unsafe { (buf as *mut i32).write_unaligned(crate::vfs::vcmux::kb_mode(vc) as i32) };
                0
            }
            // KDGKBTYPE <- unsigned char: the keyboard model the console reports.
            // A read — the caller hands a one-byte buffer expecting it filled —
            // so it cannot fall through to the setter accept below, which would
            // answer success while leaving that byte whatever the stack held.
            // Linux's answer is the constant `KB_101`, its header noting "this is
            // what we always answer" (`/usr/include/linux/kd.h`: the AT/ps2 model
            // id the driver reports regardless of what is actually plugged in), so
            // this kernel gives the same one.
            0x33 => {
                if !buffer_ok(buf, 1, true, from_user) {
                    return err::EFAULT;
                }
                unsafe {
                    (buf as *mut u8).write_unaligned(2 /* KB_101 */)
                };
                0
            }
            // The rest of the `K` family is PC-console state this kernel does not
            // own: the LED lights (`KDGETLED`/`KDSETLED`) and flags
            // (`KDGKBLED`/`KDSKBLED`), the speaker tone (`KDMKTONE`/`KIOCSOUND`),
            // the video-plane iob and map/unmap (`KDADDIO`..`KDUNMAPDISP`), the
            // font, colour-map and screen/scatter maps (`GIO_FONT`..`PIO_CMAP`),
            // the meta-key handling and the keymap/diacritic tables
            // (`KDGKBMETA`..`KDSKBDIACRUC`). There is no 8042 keyboard controller
            // here, no loadable console font or keymap, and the screen belongs to
            // DRM — so Linux's *reads* for these return live per-console state this
            // kernel has nowhere to source, and its *writes* would take effect on
            // hardware that is not present. The old catch-all returned `0` for all
            // of them, the §4.2 success-for-a-thing-not-done in its worst shape.
            // §4.4 says refuse what the machine cannot do: a number inside the `K`
            // command block is a real console ioctl this device declines
            // (`EOPNOTSUPP`), one outside it is not a console ioctl (`ENOTTY`).
            _ => {
                if matches!(nr, 0x2f..=0x71 | 0xfa..=0xfb) {
                    err::EOPNOTSUPP
                } else {
                    err::ENOTTY
                }
            }
        }
    }

    /// The console's `'V'`-typed ioctls: the family a seat manager drives the
    /// screen through - which console is on it, whether a compositor may hold
    /// it, and the `VT_PROCESS` negotiation that hands it over.
    ///
    /// The ABI layer resolved the descriptor to this tty, which is a boundary
    /// question (a bad fd is `EBADF`, a node that is not a console is `ENOTTY`),
    /// and handed down who is asking. Everything else - which console this is,
    /// what its mode is, and which requests it declines - is the console's own
    /// to answer, so it lives here on the device rather than in the syscall
    /// switchboard (`ARCHITECTURE.md` V-2, the same move the `'K'` and `'E'`
    /// families took).
    ///
    /// Both machines since `PLAN.md` §13.16: the `VT_*` handover drives
    /// `vfs::vcmux`, which was x86_64's for naming `drivers` directly and now
    /// reaches the display through `crate::console`'s `display_*` seams.
    #[cfg(target_os = "none")]
    pub fn vt_ioctl(&self, caller: VtCaller, request: u32, buf: usize, from_user: bool) -> isize {
        use crate::err;
        use crate::uaccess::buffer_ok;

        if (request >> 8) & 0xff != b'V' as u32 {
            return err::ENOTTY;
        }
        let size = ((request >> 16) & 0x3fff) as usize;
        let nr = request & 0xff;
        // `VT_OPENQRY` and `VT_GETSTATE` only ask where things are, and Linux lets
        // any tty's descriptor read those; the rest either write a console's mode,
        // answer for one, or move the screen.
        let owns_it = || self.is_foreground(caller.task) || caller.root;
        match nr {
            // VT_OPENQRY -> int: the next console a second session could take. The
            // machine has six, so "there is nowhere else to put you" is not the
            // honest answer while any of them is unclaimed; ENXIO is what Linux
            // says when all of them are taken.
            0x00 => {
                let Some(free) = crate::vfs::vcmux::free_console() else {
                    return err::ENXIO;
                };
                if !buffer_ok(buf, 4, true, from_user) {
                    return err::EFAULT;
                }
                unsafe { (buf as *mut i32).write_unaligned(free as i32) };
                0
            }
            // VT_GETMODE -> struct vt_mode { mode, waitv, relsig, acqsig, frsig }.
            // Read back what `VT_SETMODE` wrote, because a seat manager asks this to
            // find out whether the console it has is already promised to someone -
            // and because a mode the kernel accepted and forgot is a mode nobody can
            // rely on. `size` is the caller's `sizeof(struct vt_mode)`, so an older,
            // shorter struct gets the prefix it sized for rather than a fault.
            0x01 => {
                let Some(vc) = self.vc_number() else {
                    return err::EINVAL;
                };
                let out = crate::vfs::vcmux::vt_mode(vc).to_bytes();
                let n = size.min(out.len());
                if !buffer_ok(buf, n, true, from_user) {
                    return err::EFAULT;
                }
                unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), buf as *mut u8, n) };
                0
            }
            // VT_SETMODE <- struct vt_mode. `_IO`, so the struct is read from the
            // request's own size rather than from a nonzero one: the eight bytes a
            // caller that wrote `<linux/vt.h>`'s struct gives, or fewer.
            0x02 => {
                if !owns_it() {
                    return err::EPERM;
                }
                let Some(vc) = self.vc_number() else {
                    return err::EINVAL;
                };
                let n = size.min(8);
                if !buffer_ok(buf, n, false, from_user) {
                    return err::EFAULT;
                }
                let mut bytes = [0u8; 8];
                unsafe { core::ptr::copy_nonoverlapping(buf as *const u8, bytes.as_mut_ptr(), n) };
                let mode = crate::term::vtmode::VtMode::from_bytes(&bytes);
                // The promise belongs to the process writing it, and is remembered as
                // that: `relsig`/`acqsig` are sent to this pid and to no one else,
                // which is Linux's `vc->vt_pid` and what lets a seat manager set the
                // mode on a console it is about to close.
                match crate::vfs::vcmux::set_vt_mode(vc, mode, caller.tgid) {
                    Ok(()) => 0,
                    // A console that is not one of ours, or a mode that is not a
                    // mode: Linux has one answer for both.
                    Err(_) => err::EINVAL,
                }
            }
            0x03 => {
                // VT_GETSTATE -> struct vt_stat { v_active, v_signal, v_state }
                // (three shorts), and Linux writes exactly two of them.
                // `v_active` is the console on the screen - what a session reads
                // back to know where it has landed, so it is the console actually
                // shown rather than a constant that happens to be right until
                // something switches - and `v_state` is the bit mask of the
                // consoles in use, bit 0 always set because `/dev/tty0` is always
                // open. `v_signal` is **not written**: it is the `SIGIO` number a
                // console promises to send when it loses the screen, which this
                // kernel does not deliver, and Linux leaves the caller's word
                // alone. Measured on Linux 7.2.8 with all three shorts pre-filled
                // `0xAAAA`: `v_signal` came back `0xAAAA`, `v_active` 1 and
                // `v_state` 67 (`PLAN.md` §13.18).
                //
                // This arm used to write the *video mode* in the first short and
                // the console number in the second, so a seat manager reading
                // `v_active` got a mode and one reading `v_signal` got a console.
                if !buffer_ok(buf, size_of::<[u16; 3]>(), true, from_user) {
                    return err::EFAULT;
                }
                let shown = crate::term::vc::active();
                unsafe {
                    let p = buf as *mut u16;
                    p.write_unaligned(shown as u16);
                    // `p.add(1)` - `v_signal` - deliberately untouched.
                    p.add(2).write_unaligned(crate::vfs::vcmux::state_mask());
                }
                0
            }
            // VT_SENDSIG was removed from the vt driver years ago; measured on a
            // current console the request falls through the kernel's switch and
            // answers `ENOTTY`, not the `0` this arm used to return for a command
            // that does nothing anywhere.
            0x04 => err::ENOTTY,
            // VT_RELDISP: the answer to a `relsig`, and the third argument is the
            // answer itself - nonzero agrees to hand the screen over, zero refuses.
            // Only the console a release is outstanding on may answer; anything else
            // is Linux's `EINVAL`, because a session that replies to a question
            // nobody asked would be completing someone else's switch.
            0x05 => {
                if !owns_it() {
                    return err::EPERM;
                }
                let Some(vc) = self.vc_number() else {
                    return err::EINVAL;
                };
                // Success is the *answer*, not the switch: Linux's `VT_RELDISP`
                // returns 0 whether the session agreed or refused, and the
                // difference is the argument it was given. `EINVAL` is reserved for
                // the console that was never asked - which is what stops a session
                // from completing someone else's hand-over.
                if crate::vfs::vcmux::release_ack(vc, buf != 0) {
                    0
                } else {
                    err::EINVAL
                }
            }
            // VT_ACTIVATE -> int: ask for that console to be the one on the screen.
            // Linux's answer is `set_console()`: record the request, schedule the
            // console callback - which is where the releasing console's `relsig` goes
            // out - and return 0. It does not wait for the answer, and neither does
            // this; `vcmux::request_activate` says why a waiting version would
            // deadlock the seat manager that owes the answer. A number that is not one
            // of this machine's consoles is Linux's `ENXIO` (`arg == 0 || arg >
            // MAX_NR_CONSOLES`), and a session that wants to know when the screen
            // actually moved has `VT_WAITACTIVE`, the one console ioctl that waits.
            0x06 => {
                if !owns_it() {
                    return err::EPERM;
                }
                let to = buf as u32;
                if to == 0 || to > VC_COUNT {
                    return err::ENXIO;
                }
                if !crate::vfs::vcmux::request_activate(to, false) {
                    return err::ENXIO;
                }
                0
            }
            // VT_WAITACTIVE -> int: park until that console is the active one,
            // wakeable by the release that finally moves it.
            0x07 => {
                if !owns_it() {
                    return err::EPERM;
                }
                crate::vfs::vcmux::wait_active(buf as u32);
                0
            }
            0x08 => 0, // VT_DISALLOCATE: nothing to free.
            0x09 => 0, // VT_RESIZE: the window size comes from TIOCGWINSZ instead.
            0x0a => 0, // VT_RESIZEX: same.
            // VT_WAITUNTIL -> int: Linux's `vt_waitrelease`, and its two answers
            // are one short: `arg == 0` means "nothing to wait for" and returns 0
            // without looking at the screen; anything else parks in exactly
            // `VT_WAITACTIVE`'s way, and a console this machine does not have is
            // `ENXIO` (`vt_waitactive`'s own range check).
            0x0c => {
                if !owns_it() {
                    return err::EPERM;
                }
                if buf == 0 {
                    return 0;
                }
                if buf as u32 > VC_COUNT {
                    return err::ENXIO;
                }
                crate::vfs::vcmux::wait_active(buf as u32);
                0
            }
            _ => err::ENOTTY,
        }
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

    /// Whether an input byte has landed since `base` was read. The wake predicate
    /// a clock-restarting non-canonical read parks on, so a partial byte that
    /// re-anchors its `VTIME` clock ends the park instead of being swallowed.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn input_moved(&self, base: u64) -> bool {
        critical_section::with(|cs| self.ld.borrow(cs).borrow().input_count() != base)
    }

    /// Blocking read: in canonical mode at most one line, in raw mode as much
    /// as the `VMIN`/`VTIME` table allows, and 0 once input has ended.
    ///
    /// It answers `EINTR` when a signal the caller can catch ends the wait.
    /// That is Linux's answer - measured for `SIGALRM`, `SIGWINCH` and
    /// `SIGCHLD` alike, with the handler run before the return - and it is the
    /// only way a full-screen program's own timeout breaks a read at all:
    /// without it the signal woke the task, found the queue still empty, and
    /// sent it back to sleep forever. `vim` and busybox `vi` both wait on
    /// exactly this, which is where "it drew the screen and then stopped
    /// responding to everything" came from.
    #[cfg(target_os = "none")]
    pub fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let id = crate::sched::current();
        // The waiter is registered inside `read_blocking`, re-armed on every
        // pass, rather than once here - see the reason there.
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
    pub fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        let _ = buf;
        panic!("blocking tty reads need the kernel scheduler; test LineDiscipline instead");
    }

    /// Take what a read would take, without blocking and without waiting.
    ///
    /// `None` means there was nothing to take; the empty `Some(0)` is the end
    /// of input, which is what [`LineDiscipline::take`] means by it. Both the
    /// kernel's wait loop and the host's tests need exactly this - the tests
    /// cannot park a task but still want to see a typed line arrive whole at
    /// the slave - and sharing it is what keeps the two halves from drifting
    /// apart on the one question this read answers.
    #[cfg(any(test, target_os = "none"))]
    pub(crate) fn take_now(&self, buf: &mut [u8]) -> Option<usize> {
        critical_section::with(|cs| {
            let mut ld = self.ld.borrow(cs).borrow_mut();
            let bytes = ld.take(buf.len())?;
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            Some(n)
        })
    }

    #[cfg(target_os = "none")]
    fn read_blocking(&self, buf: &mut [u8]) -> VfsResult<usize> {
        let id = crate::sched::current();
        // The read's own start, which is the anchor for the `VMIN == 0` clock
        // and only for that one: with `VMIN > 0` the clock runs between bytes
        // and the discipline is where that stamp lives.
        let entry = crate::hal::ticks();
        // One decisecond, in ticks - the unit `VTIME` is written in.
        let ticks_per_ds = crate::hal::TICK_HZ as u64 / 10;
        let nonblocking = self.is_nonblocking();
        loop {
            // Re-arm the waiter on every pass, not once. `receive_byte` wakes
            // by taking the WHOLE waiter list, so a reader that loses a line to
            // another reader has already been removed from it; if it parked
            // again without re-registering, the next line would not name it and
            // it would hang. pipe and eventfd re-arm across their own retries
            // for the same reason; the `retain` keeps the list to one entry.
            critical_section::with(|cs| {
                let mut w = self.waiters.borrow(cs).borrow_mut();
                w.retain(|&x| x != id);
                w.push(id);
            });
            // The wait is re-decided on every pass, because every pass may have
            // changed the two things it depends on: the settings (a program
            // switches to raw mode between reads) and the bytes that arrived.
            let (wait, clock_base) = critical_section::with(|cs| {
                let ld = self.ld.borrow(cs).borrow();
                // A clock-restarting non-canonical read parks until a byte
                // changes this count, not only until a read completes. One
                // partial byte does not satisfy `VMIN`, so the wake that
                // re-anchors the inter-byte `VTIME` clock would otherwise be
                // swallowed by `block_until_deadline`'s condition loop and the
                // read would sleep past its own deadline. `None` keeps the
                // completed-read wake for canonical and `VTIME == 0` reads. See
                // `PLAN.md` §12.4a.
                let base = if ld.byte_restarts_clock() {
                    Some(ld.input_count())
                } else {
                    None
                };
                (ld.read_wait(entry, ticks_per_ds, nonblocking), base)
            });
            let deadline = match wait {
                ReadWait::Empty => return Ok(0),
                // `O_NONBLOCK` on a read whose answer would have been a wait.
                // Nothing has been consumed, and `EAGAIN` is what tells a
                // caller with an event loop to come back - which is the whole
                // reason it opened the terminal that way instead of parking in
                // a read that a keystroke may never end.
                ReadWait::WouldBlock => return Err(VfsError::Again),
                ReadWait::Ready => {
                    if let Some(n) = self.take_now(buf) {
                        return Ok(n);
                    }
                    continue;
                }
                ReadWait::UntilInput => u64::MAX,
                ReadWait::UntilTick(due) => {
                    if crate::hal::ticks() >= due {
                        // The clock ran out with a decision owed. Nothing here
                        // needs to know *which* clock case it is: an empty queue
                        // at this point can only be the `VMIN == 0` one, because
                        // `VMIN > 0` with an empty queue answers `UntilInput`
                        // instead - so `0` is the timeout's own answer and never
                        // a stolen EOF.
                        return Ok(self.take_now(buf).unwrap_or(0));
                    }
                    due
                }
            };
            let ended = match clock_base {
                Some(base) => crate::sched::block_until_deadline(
                    || self.has_ready() || self.input_moved(base),
                    deadline,
                ),
                None => crate::sched::block_until_deadline(|| self.has_ready(), deadline),
            };
            match ended {
                crate::sched::WaitEnd::Interrupted => return Err(VfsError::Interrupted),
                // Woken by input, or by the clock: the pass above re-reads both,
                // and a wake from anything else is simply another look.
                crate::sched::WaitEnd::Condition | crate::sched::WaitEnd::Deadline => {}
            }
        }
    }
}

/// How a terminal write reaches its wire, which decides what it may do when the
/// wire is full.
///
/// The one thing that separates the two is whether the caller may *sleep*, and
/// it is the call site - not a flag read from the CPU - that knows: `IF` is
/// clear in an interrupt handler and equally clear inside every syscall
/// (`MSR_FMASK` masks it on `syscall` entry), so asking the CPU answered
/// "interrupt" for every userland `write(2)` and let it past the pty's buffer
/// bound entirely.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// A task's own write: `write(2)`, or the terminal's answer to a cursor
    /// query. A pty's master buffer is bounded and a full one ends the wait
    /// with a partial count, `EAGAIN` or `EINTR`.
    Task,
    /// The echo of a byte that has just arrived, running in the interrupt that
    /// carried it (or in the syscall that typed it at a master). It cannot
    /// park, so it pushes and moves on.
    Interrupt,
}

/// `ONLCR`: every line feed goes out as carriage return followed by line feed.
///
/// The carriage return is *inserted*, not appended: it is what puts the cursor
/// back in column zero before the line feed moves it down, which is the whole
/// difference between starting a new line and stepping to the right. A `\r` the
/// program sent itself is left alone - `OCRNL` is the flag that would rewrite
/// one, and nothing sets it.
fn newline_expanded(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.iter().filter(|&&b| b == LF).count());
    for &byte in bytes {
        if byte == LF {
            out.push(CR);
        }
        out.push(byte);
    }
    out
}

/// How many bytes of an `ONLCR`-expanded buffer the wire's first `written`
/// bytes came from - the inverse of [`newline_expanded`], for the one caller
/// that has to answer in the writer's own units: a `write(2)` to a pty whose
/// master buffer filled part-way.
///
/// Whole input bytes only, so a boundary inside an expanded pair answers the
/// byte *before* it: the caller retries that byte, its carriage return lands
/// twice, and the line renders exactly as it would have. Counting it instead
/// would leave the line feed in the writer's hands to lose.
fn input_prefix(input: &[u8], written: usize) -> usize {
    let mut out = 0;
    for (i, &byte) in input.iter().enumerate() {
        out += if byte == LF { 2 } else { 1 };
        if out > written {
            return i;
        }
    }
    input.len()
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

/// The alias names keep one shared mode, the way Linux's single `/dev/tty`
/// and `/dev/tty0` inodes do: a `chmod` on the name sticks for every process
/// that opens it, and `stat` reports what was set.
static CONTROLLING_PERMS: super::devfs::NodePerms = super::devfs::NodePerms::new(0o666, 0, 0);
static TTY0_PERMS: super::devfs::NodePerms = super::devfs::NodePerms::new(0o666, 0, 0);

impl Inode for ControllingTty {
    fn keeps_modes(&self) -> bool {
        CONTROLLING_PERMS.keeps()
    }

    fn mode(&self) -> u32 {
        CONTROLLING_PERMS.mode()
    }

    fn set_mode(&self, mode: u32) {
        CONTROLLING_PERMS.set_mode(mode);
    }

    fn owner(&self) -> (u32, u32) {
        CONTROLLING_PERMS.owner()
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        CONTROLLING_PERMS.set_owner(uid, gid);
    }
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

/// `/dev/tty0`: the virtual console currently on the screen.
///
/// The name carries Linux's own device number - `(4, 0)`, "the current VT" -
/// and `stat` must keep reporting *that*, because it is how a caller tells the
/// alias apart from the console it happens to point at. What the *descriptor*
/// becomes is the other half: Linux's `tty_open` for driver 0, inode 0 resolves
/// the alias through `vt_consalloc` and hands back the foreground console's own
/// `tty_struct`, so a seat manager that opens `/dev/tty0` finds itself holding
/// `/dev/tty3` - and `ttyname(3)` on that descriptor says `tty3`, not `tty0`.
///
/// mpos used to register the console tty under this name, which made every
/// `/dev/tty0` descriptor a second `(4, 64)`: `ttyname` said `ttyS0`, and the
/// foreground-process-group rule a VT's ownership answers by was unreadable
/// because no terminal that number named had a foreground at all.
pub struct Tty0;

impl Inode for Tty0 {
    fn keeps_modes(&self) -> bool {
        TTY0_PERMS.keeps()
    }

    fn mode(&self) -> u32 {
        TTY0_PERMS.mode()
    }

    fn set_mode(&self, mode: u32) {
        TTY0_PERMS.set_mode(mode);
    }

    fn owner(&self) -> (u32, u32) {
        TTY0_PERMS.owner()
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        TTY0_PERMS.set_owner(uid, gid);
    }
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

    /// Linux's number for the name, kept distinct from the console's `(4, 64)`
    /// so `stat` can tell the two apart even though they open the same way.
    fn rdev(&self) -> Option<(u32, u32)> {
        Some((4, 0))
    }

    /// The open lands on the console currently shown. A host build has no
    /// console mux and no foreground console to name, which is the same answer
    /// [`ControllingTty`] gives for the same reason.
    #[cfg(target_os = "none")]
    fn open_replacement(&self) -> VfsResult<Option<Arc<dyn Inode>>> {
        let shown = crate::term::vc::active();
        let tty = super::pty::lookup(shown)
            .map(|pty| pty.slave().clone())
            .unwrap_or_else(global);
        Ok(Some(tty as Arc<dyn Inode>))
    }

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

    fn keeps_modes(&self) -> bool {
        self.perms.keeps()
    }

    fn mode(&self) -> u32 {
        self.perms.mode()
    }

    fn set_mode(&self, mode: u32) {
        self.perms.set_mode(mode);
    }

    fn owner(&self) -> (u32, u32) {
        self.perms.owner()
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        self.perms.set_owner(uid, gid);
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
        self.read(buf)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        // Output can be input: a shell that asks where the cursor is gets
        // its answer here, and a reader waiting on that answer has to be
        // woken exactly as if the bytes had been typed - but only where this
        // kernel is the terminal the question was asked of.
        let attached = self.attached();
        let (forwarded, waiters) = critical_section::with(|cs| {
            let mut ld = self.ld.borrow(cs).borrow_mut();
            let forwarded = ld.note_output(buf, &attached);
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
        let written = match forwarded {
            // The discipline answered the query itself, so every byte the
            // caller wrote was dealt with and what travelled on is the answer.
            Some(bytes) => {
                self.write_out(&bytes, Wire::Task)?;
                buf.len()
            }
            // The caller's own bytes, and the wire's count back: a pty whose
            // master buffer filled reports the part it took, and one that took
            // nothing at all reports the errno that ended the wait.
            None => self.write_out(buf, Wire::Task)?,
        };
        #[cfg(target_os = "none")]
        for id in waiters {
            crate::sched::wake(id);
        }
        #[cfg(not(target_os = "none"))]
        drop(waiters);
        Ok(written)
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{Action, Attached, CR, LineDiscipline, ReadWait, Tty, TtyOutput, input_prefix};
    use crate::vfs::termios::{ECHO, ECHOCTL, ICANON, ICRNL, ISIG, VEOF, VERASE, VMIN, VTIME};

    /// The kernel is the terminal, at the size it reports: what every
    /// existing case here pins, because each of them was written while the
    /// discipline answered every question unconditionally. The forwarding
    /// half has its own cases below.
    const KERNEL: Attached = Attached::kernel(25, 80);

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
    fn unclaimed_control_bytes_are_typed_into_the_line() {
        let mut ld = fed(b"\x01\x02x\n");
        // ^A and ^B are not termios specials, so on Linux they are characters
        // of the line - and so here. `ECHOCTL` is what renders them as `^A`
        // and `^B` on the screen without that being what is in the buffer.
        assert_eq!(ld.take_echo(), b"^A^Bx\n");
        assert_eq!(lines(&mut ld), vec![b"\x01\x02x\n".to_vec()]);
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

    #[test]
    fn ctrl_z_asks_for_a_suspend_and_never_enters_the_line() {
        // The terminal's other job-control key: `VSUSP` (0x1a) is shown as
        // `^Z` and becomes a signal, never input — the same contract as the
        // interrupt, because a suspended reader's line is as much the
        // terminal's to keep as a killed reader's.
        let mut ld = LineDiscipline::new();
        let _ = ld.receive(b'v');
        assert_eq!(ld.receive(0x1a), Action::Suspend);
        assert!(!ld.has_ready(), "a suspend completes no line");
        assert_eq!(ld.take_echo(), b"v^Z\n", "the suspend is shown, not typed");

        // A program that catches SIGTSTP (as vim does) and stops itself
        // deliberately resumes to the same line it left.
        let _ = ld.receive(b'i');
        let _ = ld.receive(b'm');
        let _ = ld.receive(b'\n');
        assert_eq!(lines(&mut ld), vec![b"vim\n".to_vec()]);
    }

    #[test]
    fn ctrl_backslash_asks_for_a_quit_and_never_enters_the_line() {
        // `VQUIT` (0x1c) completes the ISIG trio: `^\` on the screen, a
        // SIGQUIT to the foreground group, and no byte for the reader.
        let mut ld = LineDiscipline::new();
        let _ = ld.receive(b'q');
        assert_eq!(ld.receive(0x1c), Action::Quit);
        assert!(!ld.has_ready(), "a quit completes no line");
        assert_eq!(ld.take_echo(), b"q^\\\n", "the quit is shown, not typed");
    }

    #[test]
    fn isig_keys_are_ordinary_bytes_once_isig_is_off() {
        // The three keys belong to `ISIG`, not to the terminal: with the flag
        // off they are input — a raw-mode reader reading them is the point of
        // turning the flag off.
        let mut ld = raw_ld();
        let mut tio = ld.termios();
        tio.lflag &= !ISIG;
        ld.set_termios(tio);
        assert_eq!(ld.receive(0x03), Action::None);
        assert_eq!(ld.receive(0x1a), Action::None);
        assert_eq!(ld.receive(0x1c), Action::None);
        assert_eq!(ld.take(8), Some(vec![0x03, 0x1a, 0x1c]));
    }

    // --- typed sequences: characters, not keys ---

    #[test]
    fn a_typed_escape_sequence_reaches_the_reader_with_the_line() {
        // An arrow at a canonical prompt is five characters like any other,
        // delivered when the line is: what `n_tty` does, and what the shells
        // and editors here do not ask of it because they read in raw mode.
        let mut ld = LineDiscipline::new();
        for &b in b"ab\x1b[A\n" {
            assert_eq!(ld.receive(b), Action::None);
        }
        // The `ESC` is *shown* as `^[` by ECHOCTL; the buffer holds the byte.
        assert_eq!(ld.take_echo(), b"ab^[[A\n");
        assert_eq!(lines(&mut ld), vec![b"ab\x1b[A\n".to_vec()]);
    }

    #[test]
    fn a_terminal_answer_typed_back_at_the_line_is_input_like_any_other() {
        // A shell asks `ESC[6n` in raw mode and the `ESC[41;5R` answer is read
        // there too. If those report bytes land at a *canonical* reader, they
        // are characters of its line - the same thing Linux hands it - not
        // something this discipline recognises and drops.
        let mut ld = LineDiscipline::new();
        for &b in b"\x1b[41;5R" {
            assert_eq!(ld.receive(b), Action::None);
        }
        ld.receive(b'\n');
        assert_eq!(lines(&mut ld), vec![b"\x1b[41;5R\n".to_vec()]);
    }

    #[test]
    fn kill_and_werase_erase_from_the_end_of_the_line() {
        // `^W` (VWERASE) takes the last word and its spaces; `^U` (VKILL) the
        // whole line. Both chop at the end, because there is no cursor - and
        // the other emacs keys (^A..^T) are not specials at all.
        let mut ld = fed(b"one two three");
        ld.receive(0x17); // ^W
        assert_eq!(ld.line(), b"one two ".to_vec());

        let mut ld = fed(b"anything");
        ld.receive(0x15); // ^U
        assert_eq!(ld.line(), Vec::<u8>::new());
    }

    // --- termios: what a shell asks for is what it gets ---

    /// A terminal in the mode a full-screen program puts it in, with the
    /// `VMIN`/`VTIME` pair it asked for. Line editing and echo are off; `^C`
    /// still interrupts, because that is a signal rule, not an editing one.
    fn raw(vmin: u8, vtime: u8) -> LineDiscipline {
        let mut ld = LineDiscipline::new();
        let mut tio = ld.termios();
        tio.lflag &= !(ICANON | ECHO);
        tio.cc[VMIN] = vmin;
        tio.cc[VTIME] = vtime;
        ld.set_termios(tio);
        ld
    }

    fn raw_ld() -> LineDiscipline {
        raw(1, 0)
    }

    /// One decisecond - the unit `VTIME` is written in - in ticks, at the
    /// kernel's own 250 Hz. `read_wait` takes it as an argument rather than
    /// reading the clock, which is what lets this table run on the host.
    const DS: u64 = 25;

    #[test]
    fn a_poll_answers_zero_bytes_without_waiting() {
        // `VMIN == 0` with `VTIME == 0` is the read that never sleeps: measured
        // to return 0 in 0 ms, and to return 0 even when bytes land a moment
        // later, because it is not waiting for them.
        let mut ld = raw(0, 0);
        assert_eq!(ld.read_wait(0, DS, false), ReadWait::Empty);
        ld.receive(b'x');
        assert_eq!(
            ld.read_wait(0, DS, false),
            ReadWait::Ready,
            "with a byte queued, the same read takes it"
        );
    }

    #[test]
    fn vmin_zero_counts_any_byte_and_times_from_the_reads_entry() {
        // This is the shape busybox `vi` and `vim` ask for to tell a bare `ESC`
        // from the start of an escape sequence: give me whatever arrives, and if
        // nothing arrives within a decisecond, tell me so with zero bytes.
        // Measured: 0 bytes after 102 ms at `VTIME=1`, and the 1 byte that
        // landed at 100 ms returned at 100 ms rather than at 300.
        let mut ld = raw(0, 1);
        assert_eq!(ld.read_wait(1_000, DS, false), ReadWait::UntilTick(1_025));
        ld.receive(b'\x1b');
        assert!(ld.has_ready(), "VMIN == 0 makes any byte a full answer");
        assert_eq!(ld.read_wait(1_000, DS, false), ReadWait::Ready);
    }

    #[test]
    fn vmin_above_one_times_from_the_last_byte_not_the_first() {
        // Measured, and the reason the clock is anchored where it is:
        // `VMIN=2/VTIME=5` with one byte arriving at 100 ms returned that byte at
        // 604 ms - one VTIME after the byte - and a *second* read then returned
        // the byte that landed at 900 ms at 1300 ms, one VTIME after that one.
        let mut ld = raw(2, 5);
        assert_eq!(
            ld.read_wait(0, DS, false),
            ReadWait::UntilInput,
            "with an empty queue there is no clock running yet, and a read must \
             not answer 0 here: that answer means end of input on this door"
        );
        ld.receive(b'1');
        ld.note_input_tick(1_000);
        assert_eq!(
            ld.read_wait(0, DS, false),
            ReadWait::UntilTick(1_125),
            "anchored on the byte, not on the read's entry"
        );
        ld.receive(b'2');
        ld.note_input_tick(1_300);
        assert_eq!(
            ld.read_wait(0, DS, false),
            ReadWait::Ready,
            "VMIN is satisfied, so the read takes everything queued"
        );
        // `VMIN` is a minimum, and the whole queue goes: measured as 5 bytes
        // returned at once for `VMIN=3`.
        let mut ld = raw(3, 5);
        for &b in b"abcde" {
            ld.receive(b);
        }
        assert_eq!(ld.read_wait(0, DS, false), ReadWait::Ready);
        assert_eq!(ld.take(64), Some(b"abcde".to_vec()));
    }

    #[test]
    fn a_clock_below_vmin_never_runs_on_an_empty_queue() {
        // The case the old `.max(1)` stood in the way of, in its other half:
        // measured, `VMIN=5/VTIME=0` with 2 bytes queued was still blocked at
        // 2 s, and `VMIN=3/VTIME=5` with 2 queued answered those 2 at 604 ms.
        // So only a *non-empty* queue below `VMIN` has a clock.
        let mut ld = raw(5, 0);
        for &b in b"12" {
            ld.receive(b);
        }
        assert_eq!(ld.read_wait(0, DS, false), ReadWait::UntilInput);
        let mut ld = raw(5, 5);
        assert_eq!(ld.read_wait(400, DS, false), ReadWait::UntilInput);
        for &b in b"12" {
            ld.receive(b);
        }
        ld.note_input_tick(900);
        assert_eq!(ld.read_wait(400, DS, false), ReadWait::UntilTick(1_025));
    }

    #[test]
    fn canonical_mode_consults_neither_vmin_nor_vtime() {
        // Measured: canonical with `VMIN=1/VTIME=5` and "abc" typed without a
        // newline was still blocked at 4 s; the line came back whole, 1200 ms
        // later, when the newline arrived. POSIX calls the pair non-canonical
        // only, and this is what that wording costs a program to learn.
        let mut ld = LineDiscipline::new();
        let mut tio = ld.termios();
        tio.cc[VMIN] = 1;
        tio.cc[VTIME] = 5;
        ld.set_termios(tio);
        for &b in b"abc" {
            ld.receive(b);
        }
        assert_eq!(ld.read_wait(0, DS, false), ReadWait::UntilInput);
        ld.receive(CR);
        assert_eq!(ld.read_wait(0, DS, false), ReadWait::Ready);
    }

    #[test]
    fn a_nonblocking_read_takes_what_is_there_and_otherwise_answers_eagain() {
        // Every line here is a measurement on Linux (2026-10-01) against a pty
        // slave with `O_NONBLOCK` set, and the two that would be easy to get
        // wrong by writing the rule from the blocking table are the `VMIN > 0`
        // ones: a nonblocking read hands back what is queued *below* the
        // minimum, because the minimum is a floor on a wait and this read is
        // not waiting.
        assert_eq!(
            raw(0, 0).read_wait(0, DS, true),
            ReadWait::Empty,
            "VMIN=0/VTIME=0 never waited, so it keeps its 0-byte answer"
        );
        assert_eq!(raw(1, 0).read_wait(0, DS, true), ReadWait::WouldBlock);
        assert_eq!(
            raw(0, 5).read_wait(0, DS, true),
            ReadWait::WouldBlock,
            "and the clock is not consulted: EAGAIN at 0 ms, not in 500"
        );
        assert_eq!(raw(1, 2).read_wait(0, DS, true), ReadWait::WouldBlock);

        let mut below_min = raw(2, 5);
        below_min.receive(b'x');
        below_min.note_input_tick(1_000);
        assert_eq!(
            below_min.read_wait(0, DS, true),
            ReadWait::Ready,
            "one byte under VMIN=2 is handed over, not waited for"
        );
        let mut floorless = raw(5, 0);
        for &b in b"12" {
            floorless.receive(b);
        }
        assert_eq!(floorless.read_wait(0, DS, true), ReadWait::Ready);

        // Canonical mode consults neither `VMIN` nor `VTIME` on Linux, and
        // `O_NONBLOCK` does not change that: what is ready is a *line*, and a
        // half-typed one is not - even with the pair set to 0/0, where a
        // non-canonical read would answer the zero-byte poll.
        let mut canonical = LineDiscipline::new();
        for &b in b"abc" {
            canonical.receive(b);
        }
        assert_eq!(
            canonical.read_wait(0, DS, true),
            ReadWait::WouldBlock,
            "a partial line is not a line"
        );
        canonical.receive(CR);
        assert_eq!(canonical.read_wait(0, DS, true), ReadWait::Ready);

        // The counterfactual, so this test cannot pass by the flag being
        // ignored: the same states without `O_NONBLOCK` decide to wait.
        assert_eq!(raw(1, 0).read_wait(0, DS, false), ReadWait::UntilInput);
        assert_eq!(raw(0, 5).read_wait(0, DS, false), ReadWait::UntilTick(125));
        let mut below_min = raw(2, 5);
        below_min.receive(b'x');
        below_min.note_input_tick(1_000);
        assert_eq!(
            below_min.read_wait(0, DS, false),
            ReadWait::UntilTick(1_125),
            "the same byte under the same settings is waited for, one VTIME"
        );
    }

    #[test]
    fn a_partial_pty_write_is_counted_in_the_writers_own_bytes() {
        // "hi\n" leaves as "hi\r\n": four bytes on the wire, three from the
        // caller - and a stop inside the pair answers the byte before it, so
        // the retry re-offers the line feed rather than dropping it.
        assert_eq!(input_prefix(b"hi\n", 4), 3);
        assert_eq!(input_prefix(b"hi\n", 3), 2);
        assert_eq!(input_prefix(b"hi\n", 2), 2);
        assert_eq!(input_prefix(b"abc", 2), 2);
        assert_eq!(input_prefix(b"abc", 0), 0);
        assert_eq!(input_prefix(b"\n\n", 2), 1);
        assert_eq!(input_prefix(b"", 0), 0);
    }

    #[test]
    fn an_answer_or_an_eof_outranks_any_clock() {
        // A shell that asked where the cursor is must get its reply, and a
        // reader at the end of its input must be told so, whatever `VMIN` says.
        let mut ld = raw(3, 5);
        ld.answer(b"\x1b[41;5R");
        assert_eq!(ld.read_wait(0, DS, false), ReadWait::Ready);
        let mut ld = LineDiscipline::new();
        let eof = ld.termios().cc[VEOF];
        assert_eq!(ld.receive(eof), Action::None);
        assert_eq!(
            ld.read_wait(0, DS, false),
            ReadWait::Ready,
            "the end of input is an answer, and the only one that repeats as 0"
        );
        assert_eq!(ld.take(8), Some(Vec::new()), "reported once");
        assert_eq!(
            ld.read_wait(0, DS, false),
            ReadWait::UntilInput,
            "and then the next read blocks again, exactly as a fresh one does"
        );
    }

    #[test]
    fn a_byte_below_vmin_still_wakes_the_waiting_reader() {
        // Without this the waiter list is left holding a reader whose clock the
        // byte just changed, and the late byte would only be delivered when a
        // *stale* deadline ran out. This is the wake that makes the measured
        // "delivered 500 ms after it arrives" happen at all.
        assert!(raw(2, 5).byte_restarts_clock());
        assert!(
            !raw(2, 0).byte_restarts_clock(),
            "with no clock there is nothing for the byte to restart"
        );
        let mut ld = LineDiscipline::new();
        ld.receive(b'a');
        assert!(
            !ld.byte_restarts_clock(),
            "canonical mode has no clock, so a typed byte is not a wake"
        );
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
    fn vreprint_redraws_the_pending_line() {
        // `^R` (VREPRINT) is the one termios special that repaints: the line
        // so far, after an erase-to-end. There is no prompt to replay, because
        // the discipline was never the one drawing between the prompt and the
        // input - a line editor in raw mode is.
        let mut ld = fed(b"hi");
        let _ = ld.take_echo();
        ld.receive(0x12); // ^R
        assert_eq!(ld.take_echo(), b"\r\x1b[Khi");
    }

    #[test]
    fn a_cursor_position_query_is_answered_on_the_input_side() {
        // What a shell sends before it draws anything, and waits for.
        let mut ld = LineDiscipline::new();
        ld.note_output(b"/ # ", &KERNEL);
        ld.note_output(b"\x1b[6n", &KERNEL);
        // One-based, and the cursor is four characters along: row 1, column 5.
        assert_eq!(ld.take(16), Some(b"\x1b[1;5R".to_vec()));

        // It is input, so a reader waiting for it is woken by it.
        assert!(!ld.has_ready(), "the answer is read once");
    }

    #[test]
    fn the_cursor_answers_where_it_actually_is() {
        let mut ld = LineDiscipline::new();
        ld.note_output(b"a line\n", &KERNEL);
        ld.note_output(b"xy", &KERNEL);
        ld.note_output(b"\x1b[6n", &KERNEL);
        assert_eq!(ld.take(16), Some(b"\x1b[2;3R".to_vec()));
    }

    /// The other half of the decision, and the half this kernel got wrong where
    /// a real terminal was downstream: a question asked of a terminal the kernel
    /// does not draw is neither answered nor swallowed.
    ///
    /// Measured on Linux against a pty pair - the slave written to in raw mode
    /// with `ECHO` off, the master read as the emulator would - all six of these
    /// arrived at the master byte for byte and none was answered, in 6 of 6 runs.
    /// Answering them here instead is what cost `fish` its output on the serial
    /// boot: the shell pads the dangling line out to the width it is *told*, so a
    /// guessed `80` in front of a window of any other width never reaches the
    /// margin, never wraps, and the `CR` and `ESC [ K` that follow erase the line
    /// the command had just printed. The terminal at the other end of the wire
    /// knows both numbers; this kernel knew neither.
    #[test]
    fn a_query_to_a_terminal_the_kernel_does_not_draw_travels_unanswered() {
        let wire = Attached::downstream();
        for (query, name) in [
            (b"\x1b[6n".as_slice(), "cursor position"),
            (b"\x1b[?6n".as_slice(), "the DEC-private cursor position"),
            (b"\x1b[5n".as_slice(), "device status"),
            (b"\x1b[c".as_slice(), "device attributes"),
            (b"\x1b]11;?\x07".as_slice(), "the background colour query"),
            (
                b"\x1bP1+q636f6c6f7273\x1b\\".as_slice(),
                "the capability query",
            ),
        ] {
            let mut ld = LineDiscipline::new();
            // Nothing is answered: no bytes queued for the reader, and nothing
            // held back from the wire either.
            let fwd = ld.note_output(query, &wire);
            assert!(
                fwd.is_none(),
                "{name}: the query must travel exactly as it came, got {fwd:?}"
            );
            assert!(
                !ld.has_ready(),
                "{name}: nothing may be queued as input by a query this kernel \
                 is not the terminal for"
            );
        }
    }

    /// And the same six, asked of a screen the kernel draws, are answered here -
    /// because nothing else can be, and a caller that is refused waits forever.
    #[test]
    fn a_query_to_a_screen_the_kernel_draws_is_answered_and_kept_off_the_wire() {
        let mut ld = LineDiscipline::new();
        ld.note_output(b"hello", &KERNEL);
        let fwd = ld.note_output(b"\x1b[6n", &KERNEL);
        assert_eq!(fwd, Some(Vec::new()), "the answered query does not travel");
        assert_eq!(ld.take(16), Some(b"\x1b[1;6R".to_vec()));

        let mut ld = LineDiscipline::new();
        assert_eq!(ld.note_output(b"\x1b[5n", &KERNEL), Some(Vec::new()));
        assert_eq!(ld.take(16), Some(b"\x1b[0n".to_vec()));
    }

    /// A cursor is counted against the same window the terminal reports, because
    /// a program reads both. Measured on the guest: `stty cols 40` moved the
    /// reported window and the cursor report kept counting to `80`.
    #[test]
    fn the_cursor_is_counted_against_the_window_the_terminal_reports() {
        let mut ld = LineDiscipline::new();
        // Fill the line of a 40-column terminal, and ask.
        let line = vec![b'x'; 40];
        ld.note_output(&line, &Attached::kernel(10, 40));
        ld.note_output(b"\x1b[6n", &Attached::kernel(10, 40));
        assert_eq!(
            ld.take(16),
            Some(b"\x1b[1;40R".to_vec()),
            "the last cell of the fortieth column, not a column of an 80-wide guess"
        );

        // The next line: the wrap happens at the margin that was reported.
        ld.note_output(b"y", &Attached::kernel(10, 40));
        ld.note_output(b"\x1b[6n", &Attached::kernel(10, 40));
        assert_eq!(ld.take(16), Some(b"\x1b[2;2R".to_vec()));

        // And a resize is felt immediately: the same discipline, a narrower screen.
        ld.note_output(b"\x1b[H", &Attached::kernel(10, 40));
        ld.note_output(&vec![b'z'; 25], &Attached::kernel(10, 12));
        ld.note_output(b"\x1b[6n", &Attached::kernel(10, 12));
        assert_eq!(
            ld.take(16),
            Some(b"\x1b[3;2R".to_vec()),
            "twelve columns is the screen now"
        );
    }

    /// The invariant the two halves of the terminal layer were split to keep: for
    /// one byte stream, the cursor the renderer moved to and the cursor a position
    /// report answers with are the *same pair*. The renderer and the line
    /// discipline parse separately - one shapes pixels, one shapes a wire - and this
    /// is what stops those two parses from being two opinions.
    ///
    /// Every stream here is a sequence the discipline used not to follow at all:
    /// the scroll region, save/restore, the alternate screen, a line feed off the
    /// bottom, a line longer than the screen.
    #[test]
    fn the_report_and_the_screen_never_disagree_about_the_cursor() {
        use crate::term::Grid;
        const ROWS: usize = 6;
        const COLS: usize = 10;
        let long = vec![b'q'; COLS * 3 + 4];
        let streams: Vec<&[u8]> = alloc::vec![
            b"hello",
            b"a\nbb\nccc",
            b"tab\there",
            b"back\x08\x08",
            &long,
            b"\x1b[3;5Hx",
            b"\x1b[H",
            b"\x1b[999;999Hx",
            b"\x1b[2J",
            b"\x1b[2J\x1b[H",
            b"\x1b[10;1",
            b"\x1b[10d\x1b[9:9H",
            b"\x1b[4;10r\x1bH",
            b"\x1b[3r\x1b[9;3H",
            // Region, save/restore and the alternate screen each have to be seen
            // *by what happens next*: a stream that ends on a position both models
            // would give would pin nothing. So each is followed by the move that
            // only the correct region / saved pair / parked cursor explains.
            b"\x1b[4;5r\x1b[4H1\n2\n3\n4",
            b"\x1b[4;5r\x1b[5Hx\n",
            b"\x1b[s\x1b[6;9H\x1b[u\x1b[B",
            b"\x1b7\x1b[6;9H\x1b8\x1b[A",
            b"\x1b[3;3H\x1b[?1049h\x1b[6;9H\x1b[?1049l",
            b"\x1b[3;3H\x1b[?1049h\x1b[2J\x1b[?1049l\x1b[B",
            b"\x1b[s\x1b[5;9H\x1b[u",
            b"\x1b7\x1b[2;2H\x1b8",
            b"\x1bM\x1bM",
            b"\x1bD\x1bD",
            b"\x1bE\x1bE",
            b"\x1b[?1049h\x1b[3;4H",
            b"\x1b[?1049hmore\x1b[?1049l",
            b"main\x1b[?1049halt\x1b[?1049l\x1b[1;1H",
            b"\x1b[C\x1b[C\x1b[D",
            b"\x1b[A\x1b[B\x1b[E\x1b[F",
            b"\x1b[5e\x1b[3a",
            b"\x1b[6n",
            b"\x1b[10b",
            b"\x1b[38;5;196mred\x1b[0m",
            b"\x1b]0;title\x07after",
            b"\x1b(B ascii",
        ];
        for stream in streams {
            let mut grid = Grid::new(ROWS, COLS);
            grid.feed(stream);
            let mut ld = LineDiscipline::new();
            ld.note_output(stream, &Attached::kernel(ROWS, COLS));
            assert_eq!(
                grid.cursor(),
                ld.screen.pos(),
                "the screen and the report disagreed after {:?} (grid {:?}, \
                 report {:?})",
                core::str::from_utf8(stream).unwrap_or("<bytes>"),
                grid.cursor(),
                ld.screen.pos()
            );
        }
    }

    #[test]
    fn a_device_status_query_gets_the_terminals_ok() {
        let mut ld = LineDiscipline::new();
        ld.note_output(b"\x1b[5n", &KERNEL);
        assert_eq!(ld.take(16), Some(b"\x1b[0n".to_vec()));
    }

    /// `set_color` and a program with no terminfo to consult ask the terminal
    /// what it is with `CSI c`. Linux's `csi_ECMA` answers that one shape, and
    /// `respond_ID()`'s answer is the VT102 ID - so that is the answer here,
    /// given on the input side and swallowed on the output side so the host
    /// cannot answer a second time and have its reply arrive as typed input.
    #[test]
    fn a_device_attributes_query_is_answered_and_not_forwarded() {
        let mut ld = LineDiscipline::new();
        let fwd = ld.note_output(b"\x1b[c", &KERNEL);
        assert_eq!(ld.take(64), Some(b"\x1b[?6c".to_vec()));
        assert!(
            fwd.is_some() && fwd.unwrap().is_empty(),
            "the query is swallowed"
        );

        // `CSI 0 c` is the same request. An unwritten parameter is `0` for a
        // question, which is not the `1` a cursor movement leaves it at.
        let mut ld = LineDiscipline::new();
        let fwd = ld.note_output(b"\x1b[0c", &KERNEL);
        assert_eq!(ld.take(64), Some(b"\x1b[?6c".to_vec()));
        assert!(fwd.is_some() && fwd.unwrap().is_empty());

        // `CSI 1 c` asks for nothing: the console answers only `0`.
        let mut ld = LineDiscipline::new();
        assert!(ld.note_output(b"\x1b[1c", &KERNEL).is_none());
        assert!(ld.take(64).is_none(), "no reply to a nonzero parameter");
    }

    /// The `linux` terminfo that every editor here reads builds its cursor
    /// capabilities out of `CSI ? Ps c`: `civis` is `\e[?25l\e[?1c`, `cnorm` is
    /// `\e[?25h\e[?0c`, `cvvis` is `\e[?25h\e[?8c`. Linux's `csi_DEC` treats
    /// them as a cursor-type *set* and answers nothing - and that is why vim
    /// leaves nothing behind, because it writes both `civis` and `cnorm` again
    /// as it exits and never reads a reply to either. An answer to a set is a
    /// reply no one consumes: it waits in the input queue as typed text for
    /// whoever reads next, which is the `9c9c` that showed up on the prompt
    /// after vim quit.
    #[test]
    fn a_cursor_type_set_is_forwarded_and_never_answered() {
        for seq in [
            b"\x1b[?1c".as_slice(),
            b"\x1b[?0c".as_slice(),
            b"\x1b[?8c".as_slice(),
        ] {
            let mut ld = LineDiscipline::new();
            assert!(
                ld.note_output(seq, &KERNEL).is_none(),
                "{seq:?} is a set, so it travels untouched"
            );
            assert!(ld.take(64).is_none(), "{seq:?} earns no reply");
        }

        // vim's own command-line pair, as it is written: one buffer, two sets.
        let mut ld = LineDiscipline::new();
        assert!(ld.note_output(b"\x1b[?25l\x1b[?1c", &KERNEL).is_none());
        assert!(ld.take(64).is_none(), "and the pair still answers nothing");
    }

    /// `CSI > c` (secondary DA) asks for an identity the Linux console does not
    /// have: `EPgt` reaches neither `csi_DEC` nor `csi_ECMA`, so it answers
    /// nothing. It travels to the wire, where a real terminal may answer it,
    /// rather than being answered here with an identity invented for it.
    #[test]
    fn a_secondary_device_attributes_query_is_forwarded_not_answered() {
        let mut ld = LineDiscipline::new();
        assert!(ld.note_output(b"\x1b[>c", &KERNEL).is_none());
        assert!(ld.take(64).is_none(), "no identity is claimed here");
    }

    /// The DEC-private marker changes nothing about `ESC [ 6 n`: Linux answers
    /// `n` from `csi_DEC` exactly as from `csi_ECMA`. Here the marker used to
    /// parse as the first parameter, so the private form travelled unanswered
    /// and a caller that asked it waited for a reply the console never sends.
    #[test]
    fn a_private_cursor_query_is_answered_once_and_not_forwarded() {
        let mut ld = LineDiscipline::new();
        ld.note_output(b"/ # ", &KERNEL);
        let fwd = ld.note_output(b"\x1b[?6n", &KERNEL);
        assert_eq!(ld.take(16), Some(b"\x1b[1;5R".to_vec()));
        assert!(
            fwd.is_some() && fwd.unwrap().is_empty(),
            "the answer and the swallow are one decision"
        );
    }

    /// A colour (`CSI 32m`) ends in `m`, not `c`: it is drawn by the host and
    /// forwarded untouched - not mistaken for a Device Attributes query.
    #[test]
    fn a_colour_sequence_is_forwarded_not_answered() {
        let mut ld = LineDiscipline::new();
        assert!(
            ld.note_output(b"\x1b[32m", &KERNEL).is_none(),
            "nothing to answer, so nothing swallowed"
        );
        assert!(ld.take(16).is_none(), "no reply is queued");
    }

    /// `set_color` and a colour-theme probe ask the terminal its background
    /// colour with an OSC query. Left unanswered, the host replies and the
    /// reply arrives as typed input - the garbled bytes behind fish's
    /// `d=cyan` parse failure. Answered and swallowed here.
    #[test]
    fn a_background_colour_query_is_answered_and_not_forwarded() {
        let mut ld = LineDiscipline::new();
        let fwd = ld.note_output(b"\x1b]11;?\x07", &KERNEL);
        assert_eq!(
            ld.take(64),
            Some(b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\".to_vec()),
            "a well-formed background reply"
        );
        assert!(
            fwd.is_some() && fwd.unwrap().is_empty(),
            "the query is swallowed"
        );

        // A colour *set* (`11;rgb:...`) is not a question: forwarded, no reply.
        assert!(
            ld.note_output(b"\x1b]11;rgb:0000/0000/0000\x07", &KERNEL)
                .is_none()
        );
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
        let fwd = ld.note_output(b"\x1bP1+q636f6c6f7273\x1b\\", &KERNEL);
        assert_eq!(
            ld.take(64),
            Some(b"\x1bP1+r\x1b\\".to_vec()),
            "an empty capability reply"
        );
        assert!(
            fwd.is_some() && fwd.unwrap().is_empty(),
            "the query is swallowed"
        );
    }

    /// The same, written in two calls. Whether a sequence is a question the
    /// discipline answers is only known when it ends, so nothing of it may
    /// travel until then: a swallow measured against one `write` let the head
    /// of a split query reach the wire, the real terminal answered the orphan,
    /// and the reply landed as typed input - which is how every `bat` prompt
    /// came back holding `]10;rgb:abab/b2b2/bfbf`.
    #[test]
    fn a_query_split_across_writes_never_reaches_the_wire() {
        let mut ld = LineDiscipline::new();
        // `bat`'s own shape: the body in one write, the BEL in the next.
        let head = ld.note_output(b"\x1b]10;?", &KERNEL);
        assert_eq!(
            head,
            Some(Vec::new()),
            "the head of an unresolved sequence is held, not sent"
        );
        assert_eq!(
            ld.note_output(b"\x07", &KERNEL),
            Some(Vec::new()),
            "and answering it drops the whole of it"
        );
        assert_eq!(
            ld.take(64),
            Some(b"\x1b]10;rgb:dcdcdc/dcdcdc/dcdcdc\x1b\\".to_vec()),
            "the answer still goes back as input"
        );

        // A sequence that turns out to be ordinary output must still arrive,
        // in order, once it resolves - holding is a delay, not a loss.
        let mut ld = LineDiscipline::new();
        assert_eq!(ld.note_output(b"\x1b[31", &KERNEL), Some(Vec::new()));
        assert_eq!(
            ld.note_output(b"m", &KERNEL),
            Some(b"\x1b[31m".to_vec()),
            "the colour set travels whole"
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
    fn an_incomplete_escape_is_a_character_and_the_line_survives() {
        // There is no sequence state on the input side to get stuck in: a
        // lone `ESC` is a byte of the line, and nothing that follows it is
        // special because of it - the same answer Linux gives.
        let mut ld = LineDiscipline::new();
        ld.receive(b'\x1b');
        ld.receive(b'x');
        assert_eq!(ld.line(), b"\x1bx".to_vec());
        ld.receive(b'\n');
        assert_eq!(lines(&mut ld), vec![b"\x1bx\n".to_vec()]);
    }

    /// A virtual console's window is the screen it is painted on, which is what
    /// stops a full-screen program laying itself out in the corner of one.
    ///
    /// The two halves matter equally: the grid is the answer when there *is*
    /// one, and the answer a terminal with no screen of its own gets must not
    /// change. Six is used as the console number because no other test here
    /// makes a screen, so the "before" half of this cannot be reading somebody
    /// else's grid.
    #[test]
    fn a_console_reports_its_screens_window_and_others_do_not() {
        let console = Tty::new((4, 6), TtyOutput::Discard);
        // No screen yet - a headless machine, or before the render task has
        // made one - answers what Linux answers for a terminal nobody has
        // sized: nothing. A number here would be a guess a full-screen program
        // believes.
        assert_eq!(console.window_size(), (0, 0));

        crate::term::make_screen(6, 50, 160);
        assert_eq!(console.window_size(), (50, 160));

        // A `/dev/ptmx` pair is not a console however many screens exist, so
        // what it reports is what `TIOCSWINSZ` set - which is the only way a
        // pty's size can be known at all, its emulator being in userspace.
        let pair = Tty::new((136, 3), TtyOutput::Discard);
        assert_eq!(pair.window_size(), (0, 0));
        assert!(pair.set_window_size(30, 100), "a new size is a change");
        assert_eq!(pair.window_size(), (30, 100));
        assert!(!pair.set_window_size(30, 100), "setting it again is not");

        // A console stores a set size and goes on answering its screen: the
        // screen is what is being painted, and a number that disagreed with it
        // would be the same lie the corner-drawing bug came from.
        assert!(console.set_window_size(30, 100));
        assert_eq!(console.window_size(), (50, 160));
    }
}
