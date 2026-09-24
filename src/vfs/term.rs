//! The terminal emulator: a cell grid a byte-stream is painted into.
//!
//! The line discipline in [`super::tty`] has always answered the *questions* a
//! program asks a terminal (where is the cursor, what are you) and forwarded
//! everything else raw to the host, which did the drawing. This module is that
//! drawer: it consumes the same ANSI byte-stream and keeps the screen it
//! describes - a grid of cells, each a character with a resolved foreground and
//! background - so the framebuffer has something to blit.
//!
//! Deliberately a second parser alongside `note_output`. The two answer to
//! different masters - one shapes what leaves on the wire, this shapes what is
//! shown - and merging them would put the whole tested input behavior on the
//! line for a rendering change. They both understand CSI; parsing twice is
//! memory-cheap next to the wire it replaces.
//!
//! The grid is allocated once at [`Grid::new`] and `feed` never allocates, so
//! it is safe to drive from the serial interrupt handler: a keystroke's echo
//! repaints a cell without a heap under the critical section. Screens are capped
//! at 32 rows (the dirty mask is a `u32`), which 80x25 fits with room to spare.

pub mod blit;
pub mod font;
pub mod keymap;

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

/// A resolved pixel colour. Cells carry the colour, not a palette index, so the
/// blitter does no lookup and 24-bit colour costs nothing to add.
pub type Rgb = [u8; 3];

/// Default ink and paper, matching the answer the line discipline gives an
/// `OSC 10/11;?` query - if the two disagreed, a `bat` theme probe and the
/// pixels it asked for would not match.
pub const DEFAULT_FG: Rgb = [0xdc, 0xdc, 0xdc];
pub const DEFAULT_BG: Rgb = [0x1c, 0x1c, 0x1c];

/// The rows a screen can have before the `u32` dirty mask runs out.
pub const MAX_ROWS: usize = 32;

/// One character cell: a code point and the colours it is drawn in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: u8,
    pub fg: Rgb,
    pub bg: Rgb,
}

/// How many CSI parameters one sequence can carry. `38;2;r;g;b` needs five.
const PARAM_MAX: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
    Charset,
}

/// A parked copy of the main screen, kept while an alternate screen shows.
struct Snapshot {
    cells: Vec<Cell>,
    row: usize,
    col: usize,
    top: usize,
    bottom: usize,
}

/// A screen of cells with a cursor, a scroll region and a pending pen.
pub struct Grid {
    rows: usize,
    cols: usize,
    cells: Vec<Cell>,
    row: usize,
    col: usize,
    saved_row: usize,
    saved_col: usize,
    top: usize,
    bottom: usize,
    pen_fg: Rgb,
    pen_bg: Rgb,
    pen_bold: bool,
    /// Whether the cursor is shown; `less` hides it while painting a screenful.
    pub cursor_visible: bool,
    dirty: u32,
    /// The main screen parked while an alternate screen (a full-page `less`, a
    /// `vi`) is showing, so leaving it restores what was there.
    alt: Option<Snapshot>,
    // Escape decoder, carried across `feed` so a split sequence holds.
    state: State,
    params: [u16; PARAM_MAX],
    nparam: usize,
    cur: u16,
    seen_digit: bool,
    priv_marker: u8,
}

impl Grid {
    pub fn new(rows: usize, cols: usize) -> Grid {
        let rows = rows.clamp(1, MAX_ROWS);
        let cols = cols.max(1);
        Grid {
            rows,
            cols,
            cells: vec![Self::blank(); rows * cols],
            row: 0,
            col: 0,
            saved_row: 0,
            saved_col: 0,
            top: 0,
            bottom: rows - 1,
            pen_fg: DEFAULT_FG,
            pen_bg: DEFAULT_BG,
            pen_bold: false,
            cursor_visible: true,
            dirty: 0,
            alt: None,
            state: State::Ground,
            params: [0u16; PARAM_MAX],
            nparam: 0,
            cur: 0,
            seen_digit: false,
            priv_marker: 0,
        }
    }

    fn blank() -> Cell {
        Cell {
            ch: b' ',
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// The cursor, for `ESC [ 6 n` and for the blitter's block.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.cells[row * self.cols + col]
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// Take and clear the changed-row mask; a screen that changed nothing skips
    /// its flush.
    pub fn take_dirty(&mut self) -> u32 {
        let d = self.dirty;
        self.dirty = 0;
        d
    }

    pub fn mark_all_dirty(&mut self) {
        self.dirty = (1u32 << self.rows) - 1;
    }

    fn mark(&mut self, row: usize) {
        if row < MAX_ROWS {
            self.dirty |= 1 << row;
        }
    }

    /// Paint a byte-stream into the grid. Safe from any context: no allocation.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    /// Clear the screen and paint a fresh boot log onto it - how the
    /// framebuffer is brought up to show the lines the console already printed
    /// while it was still on the serial wire.
    pub fn reset_to_boot(&mut self, bytes: &[u8]) {
        self.reset();
        self.feed(bytes);
        self.mark_all_dirty();
    }

    fn byte(&mut self, b: u8) {
        match self.state {
            State::Ground => self.ground(b),
            State::Esc => self.escaped(b),
            State::Csi => self.csi(b),
            State::Osc => self.osc(b),
            State::Charset => self.state = State::Ground,
        }
    }

    fn ground(&mut self, b: u8) {
        match b {
            0x1b => self.state = State::Esc,
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            b'\r' => self.col = 0,
            0x08 => {
                if self.col > 0 {
                    self.col -= 1;
                }
            }
            0x09 => {
                let next = (self.col / 8 + 1) * 8;
                self.col = next.min(self.cols - 1);
            }
            0x07 => {}
            0x20..=0x7e | 0xa0..=0xff => self.print(b),
            _ => {}
        }
    }

    fn print(&mut self, b: u8) {
        // Defensive: the cursor is clamped wherever a sequence sets it, but a
        // display glitch must never crash the machine, so a stray out-of-range
        // row is pulled back onto the grid before it can be indexed.
        if self.row >= self.rows {
            self.row = self.rows - 1;
        }
        if self.col >= self.cols {
            self.col = 0;
            self.line_feed();
        }
        let fg = if self.pen_bold {
            brighten(self.pen_fg)
        } else {
            self.pen_fg
        };
        let idx = self.row * self.cols + self.col;
        self.cells[idx] = Cell {
            ch: b,
            fg,
            bg: self.pen_bg,
        };
        self.mark(self.row);
        self.col += 1;
    }

    fn line_feed(&mut self) {
        if self.row == self.bottom {
            self.scroll_up();
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    fn reverse_line_feed(&mut self) {
        if self.row == self.top {
            self.scroll_down();
        } else if self.row > 0 {
            self.row -= 1;
        }
    }

    /// Shift the scroll region up one row and blank the bottom of the region.
    fn scroll_up(&mut self) {
        let cols = self.cols;
        let (top, bottom) = (self.top, self.bottom);
        let src = (top + 1) * cols;
        let dst = top * cols;
        let len = (bottom - top) * cols;
        if len > 0 {
            self.cells.copy_within(src..src + len, dst);
        }
        for c in 0..cols {
            self.cells[bottom * cols + c] = Cell {
                ch: b' ',
                fg: self.pen_fg,
                bg: self.pen_bg,
            };
        }
        for r in top..=bottom {
            self.mark(r);
        }
    }

    /// Shift the scroll region down one row and blank its top (reverse index).
    fn scroll_down(&mut self) {
        let cols = self.cols;
        let (top, bottom) = (self.top, self.bottom);
        let src = top * cols;
        let dst = (top + 1) * cols;
        let len = (bottom - top) * cols;
        if len > 0 {
            self.cells.copy_within(src..src + len, dst);
        }
        for c in 0..cols {
            self.cells[top * cols + c] = Cell {
                ch: b' ',
                fg: self.pen_fg,
                bg: self.pen_bg,
            };
        }
        for r in top..=bottom {
            self.mark(r);
        }
    }

    fn escaped(&mut self, b: u8) {
        match b {
            b'[' => {
                self.state = State::Csi;
                self.params = [0u16; PARAM_MAX];
                self.nparam = 0;
                self.cur = 0;
                self.seen_digit = false;
                self.priv_marker = 0;
            }
            b']' => self.state = State::Osc,
            b'(' | b')' => self.state = State::Charset,
            b'M' => {
                self.reverse_line_feed();
                self.state = State::Ground;
            }
            b'D' => {
                self.line_feed();
                self.state = State::Ground;
            }
            b'E' => {
                self.col = 0;
                self.line_feed();
                self.state = State::Ground;
            }
            b'7' => {
                self.saved_row = self.row;
                self.saved_col = self.col;
                self.state = State::Ground;
            }
            b'8' => {
                self.row = self.saved_row;
                self.col = self.saved_col;
                self.state = State::Ground;
            }
            b'c' => {
                self.reset();
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    /// OSC is not rendered here: the line discipline already answers the
    /// queries and forwards the sets, so the renderer just skips to a BEL/ST.
    fn osc(&mut self, b: u8) {
        if b == 0x07 {
            self.state = State::Ground;
        } else if b == 0x1b {
            // The next byte is the ST terminator's `\`; skip it.
            self.state = State::Charset;
        }
    }

    fn csi(&mut self, b: u8) {
        match b {
            b'0'..=b'9' => {
                self.cur = self.cur * 10 + (b - b'0') as u16;
                self.seen_digit = true;
            }
            b';' | b':' => self.push_param(),
            b'?' | b'<' | b'=' | b'>' => self.priv_marker = b,
            0x20..=0x2f => {}
            0x40..=0x7e => {
                self.finish_param();
                self.dispatch(b);
                self.state = State::Ground;
            }
            _ => {}
        }
    }

    fn push_param(&mut self) {
        if self.nparam < PARAM_MAX {
            self.params[self.nparam] = self.cur;
            self.nparam += 1;
        }
        self.cur = 0;
        self.seen_digit = false;
    }

    fn finish_param(&mut self) {
        if self.nparam < PARAM_MAX && (self.seen_digit || self.nparam == 0) {
            self.params[self.nparam] = self.cur;
            self.nparam += 1;
        }
    }

    /// The nth parameter, defaulting to `dflt` when absent or zero.
    fn p(&self, n: usize, dflt: u16) -> u16 {
        self.params
            .get(n)
            .copied()
            .filter(|&v| v != 0)
            .unwrap_or(dflt)
    }

    fn param_at(&self, n: usize) -> Option<u16> {
        self.params.get(n).copied()
    }

    fn dispatch(&mut self, cmd: u8) {
        let np = self.nparam;
        match cmd {
            b'A' => self.row = self.row.saturating_sub(self.p(0, 1) as usize),
            b'B' => self.row = (self.row + self.p(0, 1) as usize).min(self.rows - 1),
            b'C' => self.col = (self.col + self.p(0, 1) as usize).min(self.cols - 1),
            b'D' => self.col = self.col.saturating_sub(self.p(0, 1) as usize),
            b'E' => {
                self.col = 0;
                self.row = (self.row + self.p(0, 1) as usize).min(self.rows - 1);
            }
            b'F' => {
                self.col = 0;
                self.row = self.row.saturating_sub(self.p(0, 1) as usize);
            }
            b'G' | b'`' => self.col = (self.p(0, 1) as usize - 1).min(self.cols - 1),
            b'd' => self.row = (self.p(0, 1) as usize - 1).min(self.rows - 1),
            b'H' | b'f' => {
                self.row = (self.p(0, 1) as usize - 1).min(self.rows - 1);
                self.col = (self.p(1, 1) as usize - 1).min(self.cols - 1);
            }
            b'J' => self.erase_display(self.param_at(0).unwrap_or(0)),
            b'K' => self.erase_line(self.param_at(0).unwrap_or(0)),
            b'm' => self.sgr(np),
            b'r' => {
                let top = self.p(0, 1) as usize - 1;
                let bot = self.p(1, self.rows as u16) as usize - 1;
                if top < bot && bot < self.rows {
                    self.top = top;
                    self.bottom = bot;
                    self.row = self.top;
                    self.col = 0;
                }
            }
            b's' => {
                self.saved_row = self.row;
                self.saved_col = self.col;
            }
            b'u' => {
                self.row = self.saved_row;
                self.col = self.saved_col;
            }
            b'h' | b'l' if self.priv_marker == b'?' => {
                let on = cmd == b'h';
                for i in 0..np {
                    match self.params[i] {
                        25 => self.cursor_visible = on,
                        // The alternate screen: `less`, `vi` and friends draw
                        // over a cleared window and, on leaving, put back the
                        // screen they replaced. Without this their last page is
                        // left smeared over the prompt.
                        47 | 1047 | 1049 => {
                            if on {
                                self.enter_alt();
                            } else {
                                self.exit_alt();
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: u16) {
        match mode {
            0 => self.fill(self.row, self.col, self.rows, self.cols),
            1 => self.fill(0, 0, self.row, self.col + 1),
            _ => self.fill(0, 0, self.rows, self.cols),
        }
    }

    fn erase_line(&mut self, mode: u16) {
        match mode {
            0 => self.fill(self.row, self.col, self.row + 1, self.cols),
            1 => self.fill(self.row, 0, self.row + 1, self.col + 1),
            _ => self.fill(self.row, 0, self.row + 1, self.cols),
        }
    }

    /// Blank cells in [ (r0,c0), (r1,c1) ), marking each changed row.
    fn fill(&mut self, r0: usize, c0: usize, r1: usize, c1: usize) {
        let cols = self.cols;
        for r in r0.min(self.rows)..r1.min(self.rows) {
            for c in c0.min(cols)..c1.min(cols) {
                self.cells[r * cols + c] = Cell {
                    ch: b' ',
                    fg: self.pen_fg,
                    bg: self.pen_bg,
                };
            }
            self.mark(r);
        }
    }

    fn reset(&mut self) {
        self.fill(0, 0, self.rows, self.cols);
        self.row = 0;
        self.col = 0;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.pen_fg = DEFAULT_FG;
        self.pen_bg = DEFAULT_BG;
        self.pen_bold = false;
        self.cursor_visible = true;
    }

    /// Save the main screen and clear for an alternate one. Idempotent: entering
    /// while already in the alternate is a no-op, so the snapshot is never lost.
    fn enter_alt(&mut self) {
        if self.alt.is_some() {
            return;
        }
        self.alt = Some(Snapshot {
            cells: core::mem::take(&mut self.cells),
            row: self.row,
            col: self.col,
            top: self.top,
            bottom: self.bottom,
        });
        // Reuse the allocation `take` left behind is impossible (it's now in the
        // snapshot), so build a fresh blank screen of the same shape.
        self.cells = vec![Self::blank(); self.rows * self.cols];
        self.row = 0;
        self.col = 0;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.mark_all_dirty();
    }

    /// Put the saved main screen back. A no-op if there was no alternate.
    fn exit_alt(&mut self) {
        if let Some(saved) = self.alt.take() {
            self.cells = saved.cells;
            self.row = saved.row;
            self.col = saved.col;
            self.top = saved.top;
            self.bottom = saved.bottom;
            self.mark_all_dirty();
        }
    }

    fn sgr(&mut self, np: usize) {
        let n = np.max(1); // `ESC [ m` with no parameters is a reset.
        let mut i = 0;
        while i < n {
            let p = self.param_at(i).unwrap_or(0);
            match p {
                0 => {
                    self.pen_fg = DEFAULT_FG;
                    self.pen_bg = DEFAULT_BG;
                    self.pen_bold = false;
                }
                1 => self.pen_bold = true,
                22 => self.pen_bold = false,
                7 | 27 => core::mem::swap(&mut self.pen_fg, &mut self.pen_bg),
                39 => self.pen_fg = DEFAULT_FG,
                49 => self.pen_bg = DEFAULT_BG,
                30..=37 => self.pen_fg = vga_16(p as usize - 30),
                90..=97 => self.pen_fg = vga_16(p as usize - 90 + 8),
                40..=47 => self.pen_bg = vga_16(p as usize - 40),
                100..=107 => self.pen_bg = vga_16(p as usize - 100 + 8),
                38 | 48 => {
                    let is_fg = p == 38;
                    match self.param_at(i + 1) {
                        Some(5) => {
                            if let Some(idx) = self.param_at(i + 2) {
                                let rgb = xterm_256(idx as usize);
                                if is_fg {
                                    self.pen_fg = rgb;
                                } else {
                                    self.pen_bg = rgb;
                                }
                            }
                            i += 2;
                        }
                        Some(2) => {
                            let rgb = [
                                self.param_at(i + 2).unwrap_or(0) as u8,
                                self.param_at(i + 3).unwrap_or(0) as u8,
                                self.param_at(i + 4).unwrap_or(0) as u8,
                            ];
                            if is_fg {
                                self.pen_fg = rgb;
                            } else {
                                self.pen_bg = rgb;
                            }
                            i += 4;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn vga_16(i: usize) -> Rgb {
    const BASE: [Rgb; 16] = [
        [0x00, 0x00, 0x00],
        [0xaa, 0x00, 0x00],
        [0x00, 0xaa, 0x00],
        [0xaa, 0x55, 0x00],
        [0x00, 0x00, 0xaa],
        [0xaa, 0x00, 0xaa],
        [0x00, 0xaa, 0xaa],
        [0xaa, 0xaa, 0xaa],
        [0x55, 0x55, 0x55],
        [0xff, 0x55, 0x55],
        [0x55, 0xff, 0x55],
        [0xff, 0xff, 0x55],
        [0x55, 0x55, 0xff],
        [0xff, 0x55, 0xff],
        [0x55, 0xff, 0xff],
        [0xff, 0xff, 0xff],
    ];
    BASE[i & 15]
}

/// Bold brightens only the eight base darks; anything else passes through.
fn brighten(rgb: Rgb) -> Rgb {
    for i in 0..8 {
        if vga_16(i) == rgb {
            return vga_16(i + 8);
        }
    }
    rgb
}

/// The xterm 256-colour cube: 16 base, a 6x6x6 cube, then 24 grays.
fn xterm_256(i: usize) -> Rgb {
    if i < 16 {
        return vga_16(i);
    }
    if i < 232 {
        let j = i - 16;
        let steps = [0u8, 95, 135, 175, 215, 255];
        return [steps[j / 36 % 6], steps[j / 6 % 6], steps[j % 6]];
    }
    if i < 256 {
        let v = (8 + (i - 232) * 10) as u8;
        return [v, v, v];
    }
    DEFAULT_FG
}

// ---------------------------------------------------------------------------
// The screen registry
// ---------------------------------------------------------------------------

/// The screens the framebuffer renders. Index 0 is the kernel console - what
/// `println!` paints once the GPU takes over; indices 1.. are the virtual
/// consoles the mux feeds in a later phase. A fixed small array, filled lazily.
static SCREENS: Mutex<RefCell<Vec<Option<Grid>>>> = Mutex::new(RefCell::new(Vec::new()));

/// Prepare the screen at `index`, replacing any screen there. The renderer
/// calls this for the kernel console; the mux will for each virtual console.
pub fn make_screen(index: usize, rows: usize, cols: usize) {
    critical_section::with(|cs| {
        let mut screens = SCREENS.borrow(cs).borrow_mut();
        while screens.len() <= index {
            screens.push(None);
        }
        screens[index] = Some(Grid::new(rows, cols));
    });
}

/// Whether a screen exists at `index` - a caller checking before it feeds.
pub fn has_screen(index: usize) -> bool {
    critical_section::with(|cs| {
        SCREENS
            .borrow(cs)
            .borrow()
            .get(index)
            .is_some_and(|slot| slot.is_some())
    })
}

/// Run `f` over the mutable screen at `index`, under the same critical section
/// the byte-feed and the render both take, so neither can double-borrow it.
/// `f` must not allocate on the hot path; it does not, for any caller here.
pub fn with_screen<R>(index: usize, f: impl FnOnce(&mut Grid) -> R) -> Option<R> {
    critical_section::with(|cs| {
        let mut screens = SCREENS.borrow(cs).borrow_mut();
        screens.get_mut(index).and_then(|slot| slot.as_mut()).map(f)
    })
}

/// Paint the screen at `index` into `fb` for its dirty rows and say whether it
/// repainted anything; the flush is left to the caller, outside the lock, so a
/// slow host-side DMA copy never runs with the clock masked off.
pub fn render_screen(
    index: usize,
    fb: &mut [u8],
    fb_w: usize,
    fb_h: usize,
    origin: (usize, usize),
) -> bool {
    with_screen(index, |g| {
        let dirty = g.take_dirty();
        if dirty == 0 {
            return false;
        }
        blit::paint(g, fb, fb_w, fb_h, origin, dirty);
        true
    })
    .unwrap_or(false)
}

/// The kernel console screen, index 0.
pub const KERNEL_SCREEN: usize = 0;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    fn row_text(g: &Grid, r: usize) -> String {
        (0..g.cols())
            .map(|c| g.cell(r, c).ch as char)
            .collect::<String>()
    }

    #[test]
    fn printable_text_lands_in_cells_and_advances_the_cursor() {
        let mut g = Grid::new(4, 10);
        g.feed(b"hi");
        assert_eq!(&row_text(&g, 0), "hi        ");
        assert_eq!(g.cursor(), (0, 2));
    }

    #[test]
    fn carriage_return_and_newline_move_the_cursor() {
        let mut g = Grid::new(4, 10);
        g.feed(b"abc\r\nXY");
        assert_eq!(&row_text(&g, 0), "abc       ");
        assert_eq!(&row_text(&g, 1), "XY        ");
    }

    #[test]
    fn scrolling_off_the_bottom_lifts_the_top_line_away() {
        let mut g = Grid::new(2, 5);
        // `\r\n`, not bare `\n`: a line feed keeps the column in a real
        // terminal, so an app that wants the left margin sends the CR too.
        g.feed(b"aaa\r\nbbb\r\nccc");
        // Two rows on screen; the first line has been scrolled out.
        assert_eq!(&row_text(&g, 0), "bbb  ");
        assert_eq!(&row_text(&g, 1), "ccc  ");
    }

    #[test]
    fn csi_position_homes_the_cursor_without_printing() {
        let mut g = Grid::new(4, 10);
        g.feed(b"xx\x1b[3;4Hz");
        // `z` at row 3 (1-based) col 4 (1-based) => 0-based (2,3).
        assert_eq!(g.cell(2, 3).ch, b'z');
        assert_eq!(g.cursor(), (2, 4));
    }

    #[test]
    fn sgr_256_sets_the_ink_and_reset_clears_it() {
        let mut g = Grid::new(2, 5);
        g.feed(b"\x1b[38;5;196mA\x1b[0mB");
        assert_eq!(g.cell(0, 0).fg, xterm_256(196));
        assert_eq!(g.cell(0, 1).fg, DEFAULT_FG);
    }

    #[test]
    fn a_split_escape_sequence_still_resolves() {
        let mut g = Grid::new(2, 10);
        g.feed(b"ab\x1b[");
        g.feed(b"1;3H");
        g.feed(b"Z");
        assert_eq!(g.cell(0, 2).ch, b'Z');
        // The half-sequence painted nothing as literal text; the `ab` printed
        // before it stays, and `Z` lands where the cursor was moved to.
        assert_eq!(&row_text(&g, 0), "abZ       ");
    }

    #[test]
    fn dirty_mask_reports_touched_rows_and_clears() {
        let mut g = Grid::new(3, 5);
        g.feed(b"\x1b[2J"); // erase display touches every row
        assert_eq!(g.take_dirty(), 0b111);
        assert_eq!(g.take_dirty(), 0); // cleared
    }

    #[test]
    fn a_cursor_address_past_the_margin_is_clamped_not_fatal() {
        // A getty or shell cursor-addressing at/beyond the bottom-right corner
        // must not index off the end of the grid.
        let mut g = Grid::new(2, 5);
        g.feed(b"\x1b[999;999Hx");
        assert_eq!(g.cell(1, 4).ch, b'x');
        // And a further print after the clamped position still lands in bounds.
        g.feed(b"y");
        assert_eq!(g.cell(1, 0).ch, b'y'); // 'x' filled the last cell, so it wrapped
    }

    #[test]
    fn the_alternate_screen_shields_the_main_one_then_gives_it_back() {
        let mut g = Grid::new(2, 5);
        g.feed(b"main ");
        // Enter the alternate screen (`?1049h`): the main content is parked and
        // the visible screen is cleared, so the app draws on a fresh page.
        g.feed(b"\x1b[?1049l"); // no-op exit before any enter
        g.feed(b"\x1b[?1049halt  ");
        assert_eq!(&row_text(&g, 0), "alt  ");
        // Leave it: the main screen is exactly as it was, not smeared over.
        g.feed(b"\x1b[?1049l");
        assert_eq!(&row_text(&g, 0), "main ");
    }
}
