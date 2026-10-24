//! Grid-view terminal pane: layout math and rendering.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};

use crate::ui::grid::CELL_W;

/// Default terminal-pane width, in percent of the terminal width.
pub const DEFAULT_PANE_PCT: u16 = 60;

/// Resolve the terminal-pane width: `pct`% of `term_w`, clamped to a minimum of
/// 30 columns and a maximum of 80% of the terminal width.
pub fn effective_pane_width(term_w: u16, pct: u16) -> u16 {
    let hi = ((term_w as u32 * 4) / 5).max(30) as u16;
    ((term_w as u32 * pct as u32 / 100) as u16).clamp(30, hi)
}

/// Split `area` into (grid, pane).  The grid keeps at least `CELL_W` columns
/// when there is room for them.  The key handlers derive their grid column
/// count from this same function, so motion and render never disagree.
pub fn split_grid_pane(area: Rect, pct: u16) -> (Rect, Rect) {
    let pane_w = effective_pane_width(area.width, pct).min(area.width.saturating_sub(CELL_W));
    let grid_w = area.width - pane_w;
    (
        Rect {
            width: grid_w,
            ..area
        },
        Rect {
            x: area.x + grid_w,
            width: pane_w,
            ..area
        },
    )
}

/// Render the pane for a worktree with no terminal session: a bordered block
/// holding the create hint.  `prefix` is the prefix's display form (`^a`).
pub fn render_hint(frame: &mut Frame, area: Rect, prefix: &str) {
    let block = Block::default()
        .title(" terminal ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let hint = Paragraph::new(format!(
        "Press {prefix} c to create a terminal session for this worktree"
    ))
    .style(Style::default().fg(Color::DarkGray))
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: true })
    .block(block);
    frame.render_widget(hint, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn effective_pane_width_default_60pct() {
        assert_eq!(effective_pane_width(200, DEFAULT_PANE_PCT), 120);
        assert_eq!(effective_pane_width(100, DEFAULT_PANE_PCT), 60);
    }

    #[test]
    fn effective_pane_width_clamps() {
        // Within bounds → unchanged.
        assert_eq!(effective_pane_width(200, 18), 36);
        // Below the 30-col floor → 30.
        assert_eq!(effective_pane_width(200, 5), 30);
        // Above the 80% cap (160) → 160.
        assert_eq!(effective_pane_width(200, 100), 160);
        // Tiny terminal: 80% (24) is below the floor, so the 30 floor wins.
        assert_eq!(effective_pane_width(30, DEFAULT_PANE_PCT), 30);
    }

    #[test]
    fn split_matches_grid_width() {
        for w in [40, 80, 120, 250] {
            let (grid, pane) = split_grid_pane(Rect::new(0, 0, w, 10), DEFAULT_PANE_PCT);
            assert_eq!(grid.width + pane.width, w, "w={w}");
            assert!(grid.width >= CELL_W, "w={w}");
            assert_eq!(pane.x, grid.width, "w={w}");
        }
    }

    #[test]
    fn hint_renders_when_no_session() {
        let mut terminal = Terminal::new(TestBackend::new(80, 5)).unwrap();
        terminal
            .draw(|frame| render_hint(frame, frame.area(), "^a"))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("Press ^a c to create a terminal session for this worktree"),
            "{text}"
        );
    }
}
