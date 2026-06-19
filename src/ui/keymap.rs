//! Vim-motion navigation for the 2D worktree grid.
//!
//! # Wrap rules
//!
//! - `h` (left): if at the start of a row (col == 0), move to the last item of
//!   the previous row (i.e. index - 1, clamped to 0 if already at first item).
//!   Otherwise decrement column.
//! - `l` (right): if at the end of a row OR at the last item, move to the first
//!   item of the next row (i.e. index + 1, clamped to last item).
//!   Otherwise increment column.
//! - `j` (down): move down one full row (index + cols), clamped to last valid item.
//! - `k` (up): move up one full row (index - cols), clamped to 0.
//! - `g`: jump to first item (index 0).
//! - `G`: jump to last item (count - 1), or to (n-1) when a numeric count n is
//!   supplied via `count+G` — clamped to [0, count-1].
//!
//! The function is pure (no side-effects) and is unit-tested directly.

/// A single vim-motion action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    First,
    /// `G` with optional 1-based count prefix.
    Last {
        count: Option<usize>,
    },
}

/// Compute the next selection index after applying `motion`.
///
/// # Arguments
/// * `current`    – current zero-based index
/// * `item_count` – total number of items (must be > 0; caller must guard)
/// * `cols`       – number of columns in the grid (must be >= 1)
/// * `motion`     – the motion to apply
///
/// # Returns
/// New zero-based index, always in `[0, item_count - 1]`.
pub fn apply_motion(current: usize, item_count: usize, cols: usize, motion: Motion) -> usize {
    if item_count == 0 {
        return 0;
    }
    let cols = cols.max(1);
    let last = item_count - 1;

    match motion {
        Motion::Left => {
            // h: wrap to previous item (crosses row boundary or clamps at 0).
            current.saturating_sub(1)
        }
        Motion::Right => {
            // l: wrap to next item (crosses row boundary or clamps at last item).
            (current + 1).min(last)
        }
        Motion::Up => {
            // k: move up by one full row; clamp at 0.
            current.saturating_sub(cols)
        }
        Motion::Down => {
            // j: move down by one full row; clamp at last valid item.
            (current + cols).min(last)
        }
        Motion::First => 0,
        Motion::Last { count } => match count {
            // Plain G → jump to last item.
            None => last,
            // nG → jump to 1-based index n, clamped to [1, item_count].
            Some(n) => {
                let zero_based = n.saturating_sub(1);
                zero_based.min(last)
            }
        },
    }
}

/// Clamp a selection index whenever the item list shrinks.
pub fn clamp_selection(current: usize, item_count: usize) -> usize {
    if item_count == 0 {
        0
    } else {
        current.min(item_count - 1)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: apply a motion against a 3-column grid with 7 items.
    //
    // Layout (0-based indices):
    //   [0][1][2]
    //   [3][4][5]
    //   [6]
    fn nav(current: usize, motion: Motion) -> usize {
        apply_motion(current, 7, 3, motion)
    }

    // -----------------------------------------------------------------------
    // h / l  — left / right with row-crossing wrap
    // -----------------------------------------------------------------------

    #[test]
    fn h_moves_left_within_row() {
        assert_eq!(nav(1, Motion::Left), 0);
        assert_eq!(nav(4, Motion::Left), 3);
    }

    #[test]
    fn h_crosses_row_boundary() {
        // h at index 3 (start of row 2) moves to index 2 (end of row 1).
        assert_eq!(nav(3, Motion::Left), 2);
    }

    #[test]
    fn h_clamps_at_first_item() {
        assert_eq!(nav(0, Motion::Left), 0);
    }

    #[test]
    fn l_moves_right_within_row() {
        assert_eq!(nav(0, Motion::Right), 1);
        assert_eq!(nav(4, Motion::Right), 5);
    }

    #[test]
    fn l_crosses_row_boundary() {
        // l at index 2 (end of row 1) moves to index 3 (start of row 2).
        assert_eq!(nav(2, Motion::Right), 3);
    }

    #[test]
    fn l_clamps_at_last_item() {
        // index 6 is the last item in 7-item, 3-col grid.
        assert_eq!(nav(6, Motion::Right), 6);
    }

    // -----------------------------------------------------------------------
    // j / k  — down / up by full rows
    // -----------------------------------------------------------------------

    #[test]
    fn j_moves_down_by_cols() {
        assert_eq!(nav(0, Motion::Down), 3);
        assert_eq!(nav(1, Motion::Down), 4);
    }

    #[test]
    fn j_clamps_at_last_item_not_next_row() {
        // index 4 + 3 = 7, but last item is 6.
        assert_eq!(nav(4, Motion::Down), 6);
        // index 6 + 3 = 9, clamp to 6.
        assert_eq!(nav(6, Motion::Down), 6);
    }

    #[test]
    fn k_moves_up_by_cols() {
        assert_eq!(nav(3, Motion::Up), 0);
        assert_eq!(nav(4, Motion::Up), 1);
        assert_eq!(nav(6, Motion::Up), 3);
    }

    #[test]
    fn k_clamps_at_zero() {
        assert_eq!(nav(0, Motion::Up), 0);
        assert_eq!(nav(2, Motion::Up), 0); // 2 - 3 would underflow → 0
    }

    // -----------------------------------------------------------------------
    // g / G  — first / last
    // -----------------------------------------------------------------------

    #[test]
    fn g_jumps_to_first() {
        assert_eq!(nav(5, Motion::First), 0);
        assert_eq!(nav(0, Motion::First), 0);
    }

    #[test]
    fn capital_g_jumps_to_last() {
        assert_eq!(nav(0, Motion::Last { count: None }), 6);
        assert_eq!(nav(6, Motion::Last { count: None }), 6);
    }

    // -----------------------------------------------------------------------
    // count + G
    // -----------------------------------------------------------------------

    #[test]
    fn count_g_jumps_to_one_based_index() {
        // 1G → index 0, 3G → index 2, 7G → index 6.
        assert_eq!(nav(0, Motion::Last { count: Some(1) }), 0);
        assert_eq!(nav(0, Motion::Last { count: Some(3) }), 2);
        assert_eq!(nav(0, Motion::Last { count: Some(7) }), 6);
    }

    #[test]
    fn count_g_clamps_out_of_range() {
        // 100G on a 7-item grid → last item (index 6).
        assert_eq!(nav(0, Motion::Last { count: Some(100) }), 6);
    }

    #[test]
    fn count_g_zero_treated_as_first() {
        // 0G: saturating_sub(1) on 0 → 0.
        assert_eq!(nav(5, Motion::Last { count: Some(0) }), 0);
    }

    // -----------------------------------------------------------------------
    // clamp_selection
    // -----------------------------------------------------------------------

    #[test]
    fn clamp_when_list_shrinks() {
        // 5 items → 3 items; selection was 4 → should become 2.
        assert_eq!(clamp_selection(4, 3), 2);
    }

    #[test]
    fn clamp_valid_selection_unchanged() {
        assert_eq!(clamp_selection(1, 5), 1);
    }

    #[test]
    fn clamp_empty_list_returns_zero() {
        assert_eq!(clamp_selection(99, 0), 0);
    }

    // -----------------------------------------------------------------------
    // Edge: single-item list
    // -----------------------------------------------------------------------

    #[test]
    fn single_item_all_motions_return_zero() {
        for motion in [
            Motion::Left,
            Motion::Right,
            Motion::Up,
            Motion::Down,
            Motion::First,
            Motion::Last { count: None },
            Motion::Last { count: Some(5) },
        ] {
            assert_eq!(
                apply_motion(0, 1, 1, motion),
                0,
                "motion {motion:?} on single-item list should stay at 0"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Edge: single-column grid (degenerate column=1)
    // -----------------------------------------------------------------------

    #[test]
    fn single_column_j_k_step_by_one() {
        // cols=1: j moves +1, k moves -1 (same as l/h effectively).
        assert_eq!(apply_motion(0, 5, 1, Motion::Down), 1);
        assert_eq!(apply_motion(2, 5, 1, Motion::Up), 1);
    }
}
