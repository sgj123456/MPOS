//! The cursor: the one piece of terminal state that both the drawing and the
//! answering have to agree on.
//!
//! A program asks a terminal where its cursor is (`ESC [ 6 n`) and the terminal
//! answers with two numbers out of the same state it paints the screen from. On
//! Linux that is literal: `drivers/tty/vt/vt.c` keeps the position in `struct
//! vc_data`, which `do_con_write()` advances as it draws cells and
//! `respond_csr()` reads to answer a report, and `TIOCGWINSZ` answers
//! `vc_rows`/`vc_cols` from the very same struct. There is one state and three
//! readers of it, which is why a real console cannot tell a program a position
//! the screen does not show.
//!
//! This kernel had two. [`crate::vfs::tty`]'s line discipline kept a private
//! `Screen` - a 25x80 grid hardcoded at construction and never resized, which
//! clamped its row at the bottom instead of scrolling, did not know the scroll
//! region, save/restore or the alternate screen - and answered `6n` from that.
//! [`crate::vfs::term::Grid`] moved the cursor for the same bytes and drew the
//! cells a person actually looks at. The two parsers agreed only for the streams
//! neither of them understood differently, so a report could describe a cursor
//! that the screen did not have; measured with `fish` on the guest, the shell was
//! told `40` columns by `TIOCGWINSZ` while the answer to its cursor report was
//! still being computed against `80`.
//!
//! So the arithmetic lives here once, and both users call it. Keeping it a
//! separate type - rather than letting the discipline read `Grid`'s cursor when
//! it needs to answer - is what makes the answer *synchronous*: the bytes of a
//! query can arrive in the same `write(2)` as the output they ask about, and the
//! drawing half of this kernel consumes that stream through the console
//! multiplexer's task, some time after the write returned. A report read from
//! the renderer would therefore be a frame behind - and a shell that pads a
//! dangling line to the right margin, as `fish` does, acts on the number it got
//! *now*. One state, updated by whoever is asked, is the only shape that can be
//! both current and true.
//!
//! The rules this type implements are the drawing terminal's, not a third
//! opinion: where `Grid` and the old `Screen` disagreed, `Grid` was right because
//! it is the one that painted. The one deliberate change is the last column.
//! `Grid` let its column run to `cols` after the final cell and wrapped when the
//! next glyph arrived; this type keeps the column *on* the last cell and sets
//! [`Cursor::wrap_pending`] instead, which is ECMA-48's "automatic wrap" node and
//! what every terminal reports as its cursor: a cursor parked one cell past the
//! end of the screen is not a position, and `cols + 1` is not a column a program
//! can use. The cell a glyph lands in is unchanged either way.

/// Whether the screen has to be moved to make room for the cursor.
///
/// The renderer's half of a line feed: a scroll is a shift of cells, and only the
/// caller that owns the cells can perform it. Returning the fact rather than
/// letting the caller re-derive it from the row is what keeps the two users of
/// this type from each having their own opinion about when the screen moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// The cursor moved, or stayed; the cells are where they were.
    None,
    /// The cursor was on the bottom line of the region and moved down: the region
    /// shifts up one row and the bottom line is blanked.
    Up,
    /// Reverse index at the top line of the region: the region shifts down one
    /// row and the top line is blanked.
    Down,
}

/// The saved position, as `ESC 7` / `ESC [ s` leave it and `ESC 8` / `ESC [ u`
/// take it back. A cursor can be saved before the screen has one - the sequence
/// is legal at any time - so "nothing saved" restores nothing rather than
/// inventing a home position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    row: usize,
    col: usize,
    pending: bool,
    have: bool,
}

/// The state a whole screen's worth of cursor can be swapped out into, for the
/// alternate screen: `ESC [ ? 1049 h` parks the main screen's cursor and region,
/// `l` puts both back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    row: usize,
    col: usize,
    top: usize,
    bottom: usize,
    pending: bool,
}

/// A cursor on a screen of a known size, with the scroll region it moves inside.
#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    row: usize,
    col: usize,
    rows: usize,
    cols: usize,
    top: usize,
    bottom: usize,
    wrap_pending: bool,
    saved: Saved,
}

impl Cursor {
    /// A screen `rows` by `cols`, cursor home, whole screen scrollable.
    pub const fn new(rows: usize, cols: usize) -> Cursor {
        Cursor {
            row: 0,
            col: 0,
            rows,
            cols,
            top: 0,
            bottom: rows.saturating_sub(1),
            wrap_pending: false,
            saved: Saved {
                row: 0,
                col: 0,
                pending: false,
                have: false,
            },
        }
    }

    /// Where the cursor stands, 0-based. This is the number a cursor report
    /// answers with (`+1` for the one-based form the sequence asks for), and the
    /// cell the renderer paints into.
    pub fn pos(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// The scroll region, as `(first, last)` row indices. The renderer needs it
    /// to know which rows a scroll shifts.
    pub fn region(&self) -> (usize, usize) {
        (self.top, self.bottom)
    }

    /// Whether the cursor is on the last cell of a line and the next glyph will
    /// wrap first.
    pub fn wrap_pending(&self) -> bool {
        self.wrap_pending
    }

    /// The screen got a different size: pull everything back onto it.
    ///
    /// A resize is the caller's event, not this type's - the console's grid is
    /// computed from a scanout that changed, a pty's from `TIOCSWINSZ` - and both
    /// have to end up here, because the size a program is *told* and the size a
    /// cursor is counted against must be one number.
    pub fn set_size(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        self.rows = rows;
        self.cols = cols;
        self.top = 0;
        self.bottom = rows - 1;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
        self.wrap_pending = false;
    }

    /// The cell the next glyph goes in, resolving a pending wrap, and the column
    /// the cursor is on afterwards.
    ///
    /// The caller paints at `(self.row, col)` and then calls nothing: the advance
    /// is part of this call, because a cursor that moved without the screen
    /// knowing is exactly the split this module exists to close.
    pub fn print(&mut self) -> (usize, usize, Scroll) {
        let mut scroll = Scroll::None;
        if self.wrap_pending {
            self.wrap_pending = false;
            self.col = 0;
            scroll = self.line_feed();
        }
        let row = self.row;
        let col = self.col;
        if self.col + 1 >= self.cols {
            // The last cell of the line: stay on it, and wrap when something
            // else arrives - or never, if the line ends here.
            self.wrap_pending = true;
        } else {
            self.col += 1;
        }
        (row, col, scroll)
    }

    /// Carriage return: home the column, and cancel a wrap that was waiting.
    pub fn carriage_return(&mut self) {
        self.col = 0;
        self.wrap_pending = false;
    }

    /// Backspace: one cell left, staying on the line. This is a move, not an
    /// erase - what it deletes is the renderer's business, and the two differ.
    pub fn backspace(&mut self, n: usize) {
        self.col = self.col.saturating_sub(n);
        self.wrap_pending = false;
    }

    /// Tab: the next stop every eight columns, clamped to the last cell.
    pub fn tab(&mut self) {
        let next = (self.col / 8 + 1) * 8;
        self.col = next.min(self.cols - 1);
        self.wrap_pending = false;
    }

    /// Line feed: down one line inside the region, scrolling the region when the
    /// cursor was already on its last line. The column is left alone, which is
    /// what makes `CR LF` a newline and a bare `LF` a staircase.
    pub fn line_feed(&mut self) -> Scroll {
        self.wrap_pending = false;
        if self.row >= self.bottom {
            // The cursor does not leave the bottom line: the screen moves under it.
            Scroll::Up
        } else if self.row + 1 < self.rows {
            self.row += 1;
            Scroll::None
        } else {
            Scroll::None
        }
    }

    /// Reverse line feed: up one line, scrolling the region down at its top.
    pub fn reverse_line_feed(&mut self) -> Scroll {
        self.wrap_pending = false;
        if self.row <= self.top {
            Scroll::Down
        } else {
            self.row -= 1;
            Scroll::None
        }
    }

    /// Down `n` lines, staying inside the region's bottom.
    pub fn down(&mut self, n: usize) {
        self.row = (self.row + n).min(self.bottom);
        self.wrap_pending = false;
    }

    /// Up `n` lines, staying inside the region's top.
    pub fn up(&mut self, n: usize) {
        self.row = self.row.saturating_sub(n).max(self.top);
        self.wrap_pending = false;
    }

    pub fn right(&mut self, n: usize) {
        self.col = (self.col + n).min(self.cols - 1);
        self.wrap_pending = self.col + 1 >= self.cols;
    }

    pub fn left(&mut self, n: usize) {
        self.col = self.col.saturating_sub(n);
        self.wrap_pending = false;
    }

    /// `CSI H` / `CSI f`: an absolute position, 1-based, clamped onto the screen.
    pub fn move_to(&mut self, row: usize, col: usize) {
        self.row = row.saturating_sub(1).min(self.rows - 1);
        self.col = col.saturating_sub(1).min(self.cols - 1);
        self.wrap_pending = false;
    }

    /// `CSI G`: the column alone, 1-based.
    pub fn set_column(&mut self, col: usize) {
        self.col = col.saturating_sub(1).min(self.cols - 1);
        self.wrap_pending = false;
    }

    /// `CSI d`: the line alone, 1-based.
    pub fn set_line(&mut self, row: usize) {
        self.row = row.saturating_sub(1).min(self.rows - 1);
        self.wrap_pending = false;
    }

    /// Home, as `CSI H` with no parameters means it.
    pub fn home(&mut self) {
        self.move_to(1, 1);
    }

    /// `CSI r`: the scroll region, 1-based and inclusive. `false` is a request
    /// this screen refuses - an empty or out-of-range region - which the caller
    /// leaves exactly as it was, as every terminal does.
    pub fn set_region(&mut self, top: usize, bottom: usize) -> bool {
        let t = top.saturating_sub(1);
        let b = bottom.saturating_sub(1).min(self.rows - 1);
        if t >= b {
            return false;
        }
        self.top = t;
        self.bottom = b;
        true
    }

    /// `ESC 7` / `CSI s`.
    pub fn save(&mut self) {
        self.saved = Saved {
            row: self.row,
            col: self.col,
            pending: self.wrap_pending,
            have: true,
        };
    }

    /// `ESC 8` / `CSI u`. With nothing saved this moves nothing, which is the
    /// honest answer: a position the program never saved is not a position it
    /// asked for.
    pub fn restore(&mut self) {
        if self.saved.have {
            self.row = self.saved.row.min(self.rows - 1);
            self.col = self.saved.col.min(self.cols - 1);
            self.wrap_pending = self.saved.pending;
        }
    }

    /// Park the cursor and its region for an alternate screen.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            row: self.row,
            col: self.col,
            top: self.top,
            bottom: self.bottom,
            pending: self.wrap_pending,
        }
    }

    /// Put a [`Snapshot`] back, and home what the caller has just cleared - the
    /// alternate screen always starts with its cursor on the first cell.
    pub fn take_alt(&mut self) -> Snapshot {
        let now = self.snapshot();
        self.top = 0;
        self.bottom = self.rows - 1;
        self.home();
        now
    }

    pub fn resume_alt(&mut self, saved: Snapshot) {
        self.row = saved.row.min(self.rows - 1);
        self.col = saved.col.min(self.cols - 1);
        self.top = saved.top.min(self.rows - 1);
        self.bottom = saved.bottom.min(self.rows - 1);
        self.wrap_pending = saved.pending;
    }

    /// A new screen under a cursor that has never been parked: full region, home.
    pub fn reset(&mut self) {
        self.top = 0;
        self.bottom = self.rows - 1;
        self.home();
        self.wrap_pending = false;
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::{Cursor, Scroll};

    /// The rule that makes this module worth having: the column a program is
    /// told it has, and the column a cursor is counted against, are one number.
    #[test]
    fn the_size_it_reports_is_the_size_it_counts() {
        let mut c = Cursor::new(25, 80);
        assert_eq!((c.rows(), c.cols()), (25, 80));
        c.set_size(12, 40);
        assert_eq!((c.rows(), c.cols()), (12, 40));
        // A cursor the old private model left at the bottom of a 25-row screen
        // belongs on the bottom of *this* one.
        let mut c = Cursor::new(25, 80);
        c.move_to(25, 80);
        c.set_size(4, 10);
        assert_eq!(c.pos(), (3, 9));
        assert_eq!(c.region(), (0, 3));
    }

    /// A glyph in the last cell stays there until another one arrives: that is
    /// ECMA-48's wrap node, and the difference between reporting column 80 and
    /// reporting a column that does not exist.
    #[test]
    fn the_last_column_pends_its_wrap() {
        let mut c = Cursor::new(5, 3);
        for i in 0..3 {
            let (row, col, _) = c.print();
            assert_eq!((row, col), (0, i));
        }
        assert!(c.wrap_pending());
        assert_eq!(
            c.pos(),
            (0, 2),
            "the cursor is on the last cell, not past it"
        );
        // The next print goes to the next line's first cell.
        let (row, col, scroll) = c.print();
        assert_eq!((row, col, scroll), (1, 0, Scroll::None));
    }

    /// A line feed at the bottom of the region moves the screen, not the cursor.
    #[test]
    fn a_line_at_the_bottom_asks_for_a_scroll() {
        let mut c = Cursor::new(3, 10);
        c.move_to(3, 1);
        assert_eq!(c.line_feed(), Scroll::Up);
        assert_eq!(c.pos().0, 2, "the cursor stayed on the bottom line");
        assert_eq!(c.line_feed(), Scroll::Up);
        // And inside the region it only moves.
        let mut c = Cursor::new(3, 10);
        assert_eq!(c.line_feed(), Scroll::None);
        assert_eq!(c.pos().0, 1);
    }

    /// The region is what a line feed bottoms out against, so a program that set
    /// one (`less`, a status line) gets its own last line rather than the screen's.
    #[test]
    fn a_scroll_region_holds_the_cursor_inside_it() {
        let mut c = Cursor::new(10, 10);
        assert!(c.set_region(3, 6));
        assert_eq!(c.region(), (2, 5));
        c.move_to(6, 1);
        assert_eq!(c.line_feed(), Scroll::Up);
        assert_eq!(c.pos().0, 5);
        c.move_to(3, 1);
        assert_eq!(c.reverse_line_feed(), Scroll::Down);
        // A move cannot leave the region either.
        c.down(9);
        assert_eq!(c.pos().0, 5);
        c.up(9);
        assert_eq!(c.pos().0, 2);
        // An empty region is refused, and changes nothing.
        assert!(!c.set_region(4, 4));
        assert_eq!(c.region(), (2, 5));
    }

    /// Save and restore, including over a resize that moved the screen under a
    /// saved position - the answer clamps back onto what exists.
    #[test]
    fn a_saved_position_comes_back_and_a_missing_one_does_not_move_anything() {
        let mut c = Cursor::new(5, 10);
        c.restore();
        assert_eq!(c.pos(), (0, 0), "nothing was saved, so nothing moved");
        c.move_to(4, 7);
        c.save();
        c.home();
        c.restore();
        assert_eq!(c.pos(), (3, 6));
        let mut small = Cursor::new(5, 10);
        small.move_to(5, 10);
        small.save();
        small.set_size(2, 3);
        small.restore();
        assert_eq!(small.pos(), (1, 2));
    }

    /// The alternate screen: entering it homes the cursor and opens the whole
    /// screen; leaving puts the parked cursor and region back exactly.
    #[test]
    fn the_alternate_screen_parks_and_returns_the_cursor() {
        let mut c = Cursor::new(20, 40);
        c.set_region(5, 15);
        c.move_to(9, 12);
        let parked = c.take_alt();
        assert_eq!(c.pos(), (0, 0));
        assert_eq!(c.region(), (0, 19));
        c.move_to(18, 33);
        c.resume_alt(parked);
        assert_eq!(c.pos(), (8, 11));
        assert_eq!(c.region(), (4, 14));
    }

    /// `CR` between two prints cancels the wrap: a line that was filled and then
    /// returned to is still that line, and a report after it must say so.
    #[test]
    fn a_carriage_return_cancels_a_pending_wrap() {
        let mut c = Cursor::new(5, 2);
        c.print();
        c.print();
        assert!(c.wrap_pending());
        c.carriage_return();
        assert!(!c.wrap_pending());
        let (row, col, _) = c.print();
        assert_eq!((row, col), (0, 0));
    }

    /// Tab stops every eight columns, and stop zero is the last cell - a tab at
    /// the right margin does not invent a column.
    #[test]
    fn tabs_stop_every_eight_and_stop_at_the_margin() {
        let mut c = Cursor::new(5, 20);
        c.tab();
        assert_eq!(c.pos().1, 8);
        c.tab();
        assert_eq!(c.pos().1, 16);
        c.tab();
        assert_eq!(c.pos().1, 19, "clamped to the last cell");
        c.backspace(1);
        assert_eq!(c.pos().1, 18);
    }
}
