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

    /// Render the worktree grid into `area`, GROUPED by the AUTHORITATIVE ordered
    /// project list `projects` (so projects with ZERO worktrees still render a
    /// header + divider).
    ///
    /// Each project group is a header line (project name + horizontal divider)
    /// followed by that project's worktree squares wrapping at `cols`.  Groups
    /// stack vertically in `projects` order.  Selection uses a FLAT index over
    /// the worktrees concatenated IN `projects` ORDER, so the highlight lands on
    /// the correct square within its group.  A zero-worktree project shows its
    /// header plus a dim "(no worktrees)" line.
    pub fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        projects: &[String],
        worktrees: &[WorktreeView],
    ) {
        if projects.is_empty() {
            let msg = Paragraph::new(" No worktrees found.  Add a project with `A`.")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, area);
            return;
        }

        let cols = Self::cols_for_width(area.width) as u16;
        let groups = group_by_project(projects, worktrees);

        // `y` tracks the current vertical offset within `area`; `flat` tracks
        // the running flat index across all groups (matches the project-ordered
        // concatenation of `worktrees`).
        let mut y = area.y;
        let mut flat: usize = 0;

        for group in &groups {
            // Header + divider row (1 line).
            if y >= area.y + area.height {
                break;
            }
            let header_area = Rect::new(area.x, y, area.width, 1);
            render_group_header(frame, header_area, &group.project);
            y += 1;

            if group.worktrees.is_empty() {
                // Dim placeholder line for a project with no worktrees.
                if y < area.y + area.height {
                    let note = Paragraph::new("   (no worktrees)")
                        .style(Style::default().fg(Color::DarkGray));
                    frame.render_widget(note, Rect::new(area.x, y, area.width, 1));
                }
                y += 1;
                continue;
            }

            // Worktree squares for this group, wrapping at `cols`.
            let group_rows = group.len().div_ceil(cols.max(1) as usize) as u16;
            for (local, wt) in group.worktrees.iter().enumerate() {
                let col = (local as u16) % cols;
                let row = (local as u16) / cols;
                let x = area.x + col * CELL_W;
                let cell_y = y + row * CELL_H;

                // Stop this group's cells if they would overflow the area.
                if x + CELL_W > area.x + area.width || cell_y + CELL_H > area.y + area.height {
                    flat += 1;
                    continue;
                }

                let cell_area = Rect::new(x, cell_y, CELL_W, CELL_H);
                let is_selected = flat == self.selected;
                self.render_cell(frame, cell_area, wt, is_selected);
                flat += 1;
            }

            // Advance past this group's cell rows.
            y += group_rows.max(1) * CELL_H;
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
// Project grouping
// ---------------------------------------------------------------------------

/// A contiguous run of worktrees that share a project, in flat order.
pub struct ProjectGroup<'a> {
    pub project: String,
    pub worktrees: Vec<&'a WorktreeView>,
}

impl ProjectGroup<'_> {
    pub fn len(&self) -> usize {
        self.worktrees.len()
    }

    pub fn is_empty(&self) -> bool {
        self.worktrees.is_empty()
    }
}

/// Group `worktrees` into one [`ProjectGroup`] per name in the AUTHORITATIVE
/// ordered `projects` list (so a project with zero worktrees still gets an
/// empty group, in order).  Within each group the worktrees keep their relative
/// order in `worktrees`.  Concatenating the groups' worktrees in order yields
/// the project-ordered flat list a flat selection index runs over, so an index
/// maps to `(group, local index)` by walking the groups and summing lengths.
pub fn group_by_project<'a>(
    projects: &[String],
    worktrees: &'a [WorktreeView],
) -> Vec<ProjectGroup<'a>> {
    projects
        .iter()
        .map(|name| ProjectGroup {
            project: name.clone(),
            worktrees: worktrees.iter().filter(|wt| &wt.project == name).collect(),
        })
        .collect()
}

/// Map a flat selection index to `(project_name, local_index)` within its
/// group, using the same grouping as [`group_by_project`].  Returns `None` if
/// the index is out of range.
#[allow(dead_code)] // pure helper exercised by unit tests; available for callers
pub fn flat_to_group_local(
    projects: &[String],
    worktrees: &[WorktreeView],
    flat: usize,
) -> Option<(String, usize)> {
    let groups = group_by_project(projects, worktrees);
    let mut base = 0usize;
    for g in &groups {
        if !g.is_empty() && flat < base + g.len() {
            return Some((g.project.clone(), flat - base));
        }
        base += g.len();
    }
    None
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Render a project group header: the project name followed by a `─`-filled
/// divider spanning the remaining width.
fn render_group_header(frame: &mut Frame, area: Rect, project: &str) {
    if area.width == 0 {
        return;
    }
    let name = format!(" {project} ");
    let name_len = name.chars().count() as u16;
    let rule_len = area.width.saturating_sub(name_len);
    let rule: String = "─".repeat(rule_len as usize);
    let line = Line::from(vec![
        Span::styled(
            name,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(rule, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wv(project: &str, name: &str) -> WorktreeView {
        WorktreeView {
            path: PathBuf::from(format!("/wt/{project}/{name}")),
            project: project.to_string(),
            name: name.to_string(),
            branch: "HEAD".to_string(),
            prompt_slug: None,
            pr_number: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            last_summary: None,
        }
    }

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn group_by_project_follows_project_order() {
        let projects = names(&["alpha", "beta"]);
        let wts = vec![wv("alpha", "a1"), wv("alpha", "a2"), wv("beta", "b1")];
        let groups = group_by_project(&projects, &wts);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].project, "alpha");
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].project, "beta");
        assert_eq!(groups[1].len(), 1);
        assert!(!groups[0].is_empty());
    }

    #[test]
    fn group_by_project_includes_empty_project() {
        // "beta" has no worktrees but still gets a (empty) group, in order.
        let projects = names(&["alpha", "beta", "gamma"]);
        let wts = vec![wv("alpha", "a1"), wv("gamma", "g1")];
        let groups = group_by_project(&projects, &wts);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].project, "alpha");
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[1].project, "beta");
        assert!(groups[1].is_empty());
        assert_eq!(groups[2].project, "gamma");
        assert_eq!(groups[2].len(), 1);
    }

    #[test]
    fn group_by_project_single_project() {
        let projects = names(&["solo"]);
        let wts = vec![wv("solo", "x"), wv("solo", "y")];
        let groups = group_by_project(&projects, &wts);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
    }

    #[test]
    fn group_by_project_no_projects() {
        let projects: Vec<String> = vec![];
        let wts: Vec<WorktreeView> = vec![];
        assert!(group_by_project(&projects, &wts).is_empty());
    }

    #[test]
    fn flat_index_maps_to_group_local_with_empty_in_middle() {
        // "beta" (empty) sits between alpha and gamma; the flat index skips it.
        let projects = names(&["alpha", "beta", "gamma"]);
        let wts = vec![
            wv("alpha", "a1"), // flat 0 -> (alpha, 0)
            wv("alpha", "a2"), // flat 1 -> (alpha, 1)
            wv("gamma", "g1"), // flat 2 -> (gamma, 0)
            wv("gamma", "g2"), // flat 3 -> (gamma, 1)
        ];
        assert_eq!(
            flat_to_group_local(&projects, &wts, 0),
            Some(("alpha".to_string(), 0))
        );
        assert_eq!(
            flat_to_group_local(&projects, &wts, 1),
            Some(("alpha".to_string(), 1))
        );
        assert_eq!(
            flat_to_group_local(&projects, &wts, 2),
            Some(("gamma".to_string(), 0))
        );
        assert_eq!(
            flat_to_group_local(&projects, &wts, 3),
            Some(("gamma".to_string(), 1))
        );
        // Out of range.
        assert_eq!(flat_to_group_local(&projects, &wts, 4), None);
    }

    #[test]
    fn flat_concatenation_reproduces_project_ordered_input() {
        let projects = names(&["alpha", "beta"]);
        let wts = vec![wv("alpha", "a1"), wv("beta", "b1"), wv("beta", "b2")];
        let groups = group_by_project(&projects, &wts);
        let flat: Vec<&WorktreeView> = groups
            .iter()
            .flat_map(|g| g.worktrees.iter().copied())
            .collect();
        assert_eq!(flat.len(), wts.len());
        for (i, wt) in wts.iter().enumerate() {
            assert_eq!(flat[i].path, wt.path);
        }
    }
}
