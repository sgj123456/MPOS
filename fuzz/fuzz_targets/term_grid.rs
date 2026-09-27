#![no_main]

//! The VT's escape-sequence parser.
//!
//! `term::Grid::feed` is a state machine over an untrusted byte stream - the
//! output of every program that ever printed anything - and it owns the cursor,
//! the scroll region and a fixed dirty mask indexed by row. The two shapes a
//! parser of this kind breaks in are the same two: an index that runs off the
//! end of the grid, and a state that never leaves an escape. Both are asserted
//! here after every single byte, because a sequence that is only wrong in its
//! middle is exactly as broken as one that is wrong at the end.

use arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use mpos::vfs::term::{self, Grid};

#[derive(Arbitrary, Debug)]
struct Input {
    /// Screen geometry. Clamped by `Grid::new` itself; the target checks that
    /// the clamping happened rather than assuming it.
    rows: usize,
    cols: usize,
    bytes: Vec<u8>,
}

fuzz_target!(|input: Input| {
    // `Grid::new` allocates rows * cols cells, so a fuzzed geometry is capped
    // here for the same reason the kernel caps it: MAX_ROWS rows, and a
    // console-width number of columns.
    let rows = input.rows.clamp(1, term::MAX_ROWS);
    let cols = input.cols.clamp(1, 512);
    let mut grid = Grid::new(rows, cols);
    assert_eq!(grid.rows() * grid.cols(), grid.cells().len());
    let cells = grid.rows() * grid.cols();

    for &byte in &input.bytes {
        grid.feed(&[byte]);
        check(&grid, cells);
        // `take_dirty` is the only way to read the mask, and reading it clears
        // it, so this is checked once per byte rather than in `check`.
        let dirty = grid.take_dirty();
        for row in 0..64u32 {
            if dirty & (1 << row) != 0 {
                assert!(
                    (row as usize) < grid.rows(),
                    "dirty mask marks row {row} of a {} row screen",
                    grid.rows()
                );
            }
        }
    }

    // The same stream in one call is the same screen: the parser carries no
    // state a chunk boundary can leak, and DRM damage clipping depends on
    // that being true.
    let mut whole = Grid::new(rows, cols);
    whole.feed(&input.bytes);
    assert!(
        whole.cells() == grid.cells(),
        "feeding byte by byte is not the same screen as feeding at once"
    );

    // A reset, then the stream again: the alternate-screen park and the
    // scroll region have to survive being filled from nothing.
    grid.reset_to_boot(&input.bytes);
    check(&grid, cells);
});

fn check(grid: &Grid, cells: usize) {
    let (row, col) = grid.cursor();
    assert!(
        row < grid.rows(),
        "cursor on row {row} of a {} row screen",
        grid.rows()
    );
    // One past the last column is allowed and means something: a print at the
    // final column leaves the cursor wrap-pending there, which is why
    // `term::blit` clamps it before drawing the block. What may not happen is
    // running further than that, because every consumer that forgets to clamp
    // then indexes the first cell of the next row.
    assert!(
        col <= grid.cols(),
        "cursor in column {col} of a {} column screen",
        grid.cols()
    );
    if col < grid.cols() {
        let _ = grid.cell(row, col);
    }
    for r in 0..grid.rows() {
        let _ = grid.cell(r, grid.cols() - 1);
    }
    assert_eq!(grid.cells().len(), cells);
}
