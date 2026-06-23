//! Vim-motion navigation for the 2D worktree grid.
//!
//! Motion semantics (implemented layout-aware in `grid::move_in_layout`):
//!
//! - `h` (left): move to the previous cell; crosses project-group row boundaries
//!   in reading order; clamps at the first cell.
//! - `l` (right): move to the next cell; crosses project-group row boundaries;
//!   clamps at the last cell.
//! - `j` (down): move down one visual row within the same column (clamped to the
//!   last row's length); respects per-group wrapping.
//! - `k` (up): move up one visual row; clamps at the top row.
//! - `g`: jump to first item (index 0).
//! - `G`: jump to last item, or to the nth item when a numeric count prefix is
//!   supplied (`nG`) — 1-based, clamped to the valid range.

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
}
