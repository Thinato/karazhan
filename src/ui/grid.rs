use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ipc::WorktreeView;
use crate::worktree::WorktreeStatus;

use super::keymap::{apply_motion, Motion};

// ---------------------------------------------------------------------------
// Cell geometry
// ---------------------------------------------------------------------------

/// Fixed cell width (columns) for each worktree square, including borders.
pub const CELL_W: u16 = 22;
/// Fixed cell height (rows) for each worktree square, including borders.
pub const CELL_H: u16 = 5;

// ---------------------------------------------------------------------------
// GridView state
// ---------------------------------------------------------------------------

/// Holds selection state for the worktree grid.
pub struct GridView {
    /// Zero-based index of the currently selected worktree.
    pub selected: usize,
    /// Digit prefix accumulated while the user types a count before `G`.
    /// Reset after any non-digit key or after `G` consumes it.
    pub pending_count: Option<usize>,
}

impl GridView {
    pub fn new() -> Self {
        Self {
            selected: 0,
            pending_count: None,
        }
    }

    /// Push a digit into the pending count.
    pub fn push_digit(&mut self, d: u8) {
        let v = self.pending_count.unwrap_or(0);
        self.pending_count = Some(v * 10 + d as usize);
    }

    /// Clear any accumulated digit prefix.
    pub fn clear_pending_count(&mut self) {
        self.pending_count = None;
    }

    /// Apply a motion, updating `selected`.
    /// `item_count` is the current length of the worktree list.
    /// `cols` is the column count computed from the current terminal width.
    pub fn apply(&mut self, motion: Motion, item_count: usize, cols: usize) {
        if item_count == 0 {
            self.selected = 0;
            self.pending_count = None;
            return;
        }

        let motion_with_count = match motion {
            Motion::Last { .. } => Motion::Last {
                count: self.pending_count,
            },
            other => other,
        };

        self.selected = apply_motion(self.selected, item_count, cols, motion_with_count);
        self.pending_count = None;
    }

    /// Compute the number of grid columns that fit within `area_width`.
    pub fn cols_for_width(area_width: u16) -> usize {
        // At least 1 column even on very narrow terminals.
        (area_width / CELL_W).max(1) as usize
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    /// Render the worktree grid into `area`.
    ///
    /// Each worktree gets a fixed-size cell (CELL_W × CELL_H) arranged in rows.
    /// The selected cell is rendered with a double-line border and inverted text.
    pub fn render(&self, frame: &mut Frame, area: Rect, worktrees: &[WorktreeView]) {
        if worktrees.is_empty() {
            let msg = Paragraph::new(" No worktrees found.  Create one with `git worktree add`.")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, area);
            return;
        }

        let cols = Self::cols_for_width(area.width) as u16;

        for (i, wt) in worktrees.iter().enumerate() {
            let col = (i as u16) % cols;
            let row = (i as u16) / cols;

            let x = area.x + col * CELL_W;
            let y = area.y + row * CELL_H;

            // Stop rendering if the cell would go outside the available area.
            if x + CELL_W > area.x + area.width || y + CELL_H > area.y + area.height {
                break;
            }

            let cell_area = Rect::new(x, y, CELL_W, CELL_H);
            let is_selected = i == self.selected;
            self.render_cell(frame, cell_area, wt, is_selected);
        }
    }

    fn render_cell(&self, frame: &mut Frame, area: Rect, wt: &WorktreeView, selected: bool) {
        // Derive colors from status.
        let (border_color, label_color) = status_colors(&wt.status);

        let border_type = if selected {
            BorderType::Double
        } else {
            BorderType::Rounded
        };

        let border_style = if selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(border_color)
        };

        // Build inner label lines.
        // Line 1: worktree name (preferred over branch) + short 8-char dir suffix.
        let inner_w = (CELL_W.saturating_sub(4)) as usize;
        let dir_suffix: String = wt
            .path
            .file_name()
            .map(|n| n.to_string_lossy().chars().take(8).collect())
            .unwrap_or_default();
        let name_label = if dir_suffix.is_empty() {
            truncate(&wt.name, inner_w)
        } else {
            truncate(&format!("{} {}", wt.name, dir_suffix), inner_w)
        };

        // Line 2: prompt slug + PR number (combined, truncated).
        let mut meta_parts: Vec<String> = Vec::new();
        if let Some(slug) = &wt.prompt_slug {
            meta_parts.push(format!("p:{slug}"));
        }
        if let Some(pr) = wt.pr_number {
            meta_parts.push(format!("#{pr}"));
        }
        let meta_label = truncate(&meta_parts.join(" "), inner_w);

        // Status badge (short tag shown on line 3).
        let status_tag = status_tag(&wt.status);

        let label_style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(label_color)
        };

        let status_style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(label_color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::BOLD)
        };

        // Clear the cell first so overlapping cells from a previous frame don't
        // bleed through.
        frame.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(border_type)
            .border_style(border_style);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Render the text lines inside the border.
        let lines: Vec<Line> = vec![
            Line::from(Span::styled(name_label, label_style)),
            Line::from(Span::styled(meta_label, label_style)),
            Line::from(Span::styled(status_tag, status_style)),
        ];

        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);
    }
}

impl Default for GridView {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return (border_color, label_color) for a given status.
fn status_colors(status: &WorktreeStatus) -> (Color, Color) {
    match status {
        WorktreeStatus::Idle => (Color::DarkGray, Color::Gray),
        WorktreeStatus::Running => (Color::Blue, Color::Yellow),
        WorktreeStatus::NeedsReview => (Color::Magenta, Color::Magenta),
        WorktreeStatus::CIFailing => (Color::Red, Color::Red),
        WorktreeStatus::PRMerged => (Color::Green, Color::Green),
        WorktreeStatus::Error => (Color::Red, Color::Red),
    }
}

/// Short human-readable tag for a status.
fn status_tag(status: &WorktreeStatus) -> String {
    match status {
        WorktreeStatus::Idle => "idle".to_string(),
        WorktreeStatus::Running => "running…".to_string(),
        WorktreeStatus::NeedsReview => "needs review".to_string(),
        WorktreeStatus::CIFailing => "CI failing".to_string(),
        WorktreeStatus::PRMerged => "PR merged".to_string(),
        WorktreeStatus::Error => "error".to_string(),
    }
}

/// Truncate a string to at most `max` characters, appending `…` if truncated.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let take = max.saturating_sub(1);
        chars[..take].iter().collect::<String>() + "…"
    }
}
