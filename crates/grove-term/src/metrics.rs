/// Size of one terminal cell in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CellSize {
    pub width: f32,
    pub height: f32,
}

/// Columns and rows that fit in `width` × `height`; never less than 1×1.
pub(crate) fn grid_size(width: f32, height: f32, cell: CellSize) -> (u16, u16) {
    let fit = |space: f32, cell: f32| {
        if cell <= 0.0 || !space.is_finite() {
            return 1;
        }
        // A hair of tolerance so a size that is an exact multiple of the cell
        // doesn't lose a column to float rounding.
        ((space + 0.01) / cell)
            .floor()
            .clamp(1.0, f32::from(u16::MAX)) as u16
    };
    (fit(width, cell.width), fit(height, cell.height))
}

/// Maps a position relative to the grid origin to a cell, clamped to the grid.
/// Also returns how many rows the position lies above (negative) or below
/// (positive) the grid, for autoscrolling while selecting.
pub(crate) fn cell_at(x: f32, y: f32, cell: CellSize, cols: u16, rows: u16) -> (u16, u16, i32) {
    let col = (x / cell.width)
        .floor()
        .clamp(0.0, f32::from(cols.saturating_sub(1))) as u16;
    let raw_row = (y / cell.height).floor();
    let overflow = if raw_row < 0.0 {
        raw_row as i32
    } else if raw_row >= f32::from(rows) {
        raw_row as i32 - i32::from(rows) + 1
    } else {
        0
    };
    let row = raw_row.clamp(0.0, f32::from(rows.saturating_sub(1))) as u16;
    (col, row, overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: CellSize = CellSize {
        width: 8.0,
        height: 16.0,
    };

    #[test]
    fn grid_size_fits_whole_cells() {
        assert_eq!(grid_size(800.0, 480.0, CELL), (100, 30));
        assert_eq!(grid_size(807.9, 495.0, CELL), (100, 30));
        assert_eq!(grid_size(799.999, 479.999, CELL), (100, 30));
        assert_eq!(grid_size(0.0, 0.0, CELL), (1, 1));
        assert_eq!(grid_size(-5.0, 10.0, CELL), (1, 1));
        assert_eq!(grid_size(f32::INFINITY, 100.0, CELL), (1, 6));
        assert_eq!(
            grid_size(
                100.0,
                100.0,
                CellSize {
                    width: 0.0,
                    height: 0.0
                }
            ),
            (1, 1)
        );
    }

    #[test]
    fn grid_size_with_fractional_cells() {
        let menlo = CellSize {
            width: 7.83,
            height: 15.6,
        };
        assert_eq!(grid_size(783.0, 156.0, menlo), (100, 10));
    }

    #[test]
    fn cell_at_clamps_and_reports_overflow() {
        assert_eq!(cell_at(0.0, 0.0, CELL, 80, 24), (0, 0, 0));
        assert_eq!(cell_at(15.9, 31.9, CELL, 80, 24), (1, 1, 0));
        assert_eq!(cell_at(-10.0, -1.0, CELL, 80, 24), (0, 0, -1));
        assert_eq!(cell_at(10_000.0, 24.0 * 16.0, CELL, 80, 24), (79, 23, 1));
        assert_eq!(cell_at(8.0, 26.0 * 16.0, CELL, 80, 24), (1, 23, 3));
    }
}
