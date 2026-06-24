use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ipc::WorktreeView;
use crate::ui::spinner_glyph;
use crate::worktree::{PrStatus, WorktreeStatus};

use super::keymap::Motion;

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
    ///
    /// Uses the SAME per-project visual layout that `render` draws (each project
    /// group wraps independently at `cols`), so the selection always moves to the
    /// visually correct cell regardless of group sizes or terminal width.
    pub fn apply(
        &mut self,
        motion: Motion,
        projects: &[String],
        worktrees: &[WorktreeView],
        cols: usize,
    ) {
        if worktrees.is_empty() {
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

        let rows = visual_rows(projects, worktrees, cols);
        self.selected = move_in_layout(&rows, self.selected, motion_with_count);
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
        spinner_frame: usize,
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
                self.render_cell(frame, cell_area, wt, is_selected, spinner_frame);
                flat += 1;
            }

            // Advance past this group's cell rows.
            y += group_rows.max(1) * CELL_H;
        }
    }

    fn render_cell(
        &self,
        frame: &mut Frame,
        area: Rect,
        wt: &WorktreeView,
        selected: bool,
        spinner_frame: usize,
    ) {
        let running = wt.status == WorktreeStatus::Running;
        let deleting = wt.status == WorktreeStatus::Deleting;
        // Both Running and Deleting are "active" states that animate a spinner.
        let active = running || deleting;
        // BORDER + name label colors come from the agent-activity status.
        let (border_color, label_color) = status_colors(&wt.status);
        // The colored STATUS TAG line is the PR status (separate axis).
        let pr_color = pr_status_colors(&wt.pr_status);

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
        let name_core = if dir_suffix.is_empty() {
            wt.name.clone()
        } else {
            format!("{} {}", wt.name, dir_suffix)
        };
        // While active (Running/Deleting), prefix the name with an animated
        // spinner (reserving 2 cols for the glyph + space) so the user sees work.
        let name_label = if active {
            format!(
                "{} {}",
                spinner_glyph(spinner_frame),
                truncate(&name_core, inner_w.saturating_sub(2))
            )
        } else {
            truncate(&name_core, inner_w)
        };

        // Line 2: prompt slug + PR number, with an optional unresolved-comment
        // badge (` !{n}`) rendered in a distinct bright color.  The badge is shown
        // only for open PRs with n > 0 (None / 0 → nothing).
        let mut meta_parts: Vec<String> = Vec::new();
        if let Some(slug) = &wt.prompt_slug {
            meta_parts.push(format!("p:{slug}"));
        }
        if let Some(pr) = wt.pr_number {
            meta_parts.push(format!("#{pr}"));
        }
        let unresolved_badge = match wt.unresolved_comments {
            Some(n) if n > 0 => Some(format!(" !{n}")),
            _ => None,
        };
        // Reserve room for the badge so it survives truncation, then render it as
        // its own colored span.
        let badge_w = unresolved_badge
            .as_ref()
            .map(|b| b.chars().count())
            .unwrap_or(0);
        let base_w = inner_w.saturating_sub(badge_w);
        let meta_base = truncate(&meta_parts.join(" "), base_w);

        // Status badge (short tag shown on line 3) — reflects the PR status.
        let status_tag = pr_status_label(&wt.pr_status).to_string();

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
                .bg(pr_color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(pr_color).add_modifier(Modifier::BOLD)
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

        // Build the meta line: base text in the label color, plus the optional
        // unresolved-comment badge in a bright attention color.
        let badge_style = if selected {
            Style::default()
                .fg(Color::LightRed)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD)
        };
        let mut meta_spans: Vec<Span> = vec![Span::styled(meta_base, label_style)];
        if let Some(badge) = unresolved_badge {
            meta_spans.push(Span::styled(badge, badge_style));
        }

        // Line 3: while Running, show the live agent activity (e.g. "Editing
        // foo.rs"); while Deleting, show "Deleting…"; otherwise the PR status tag.
        let line3 = if running || deleting {
            let (text, fg) = if deleting {
                (truncate("Deleting…", inner_w), Color::Red)
            } else {
                (
                    truncate(wt.activity.as_deref().unwrap_or("working…"), inner_w),
                    Color::Yellow,
                )
            };
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(fg)
            };
            Span::styled(text, style)
        } else {
            Span::styled(status_tag, status_style)
        };

        // Render the text lines inside the border.
        let lines: Vec<Line> = vec![
            Line::from(Span::styled(name_label, label_style)),
            Line::from(meta_spans),
            Line::from(line3),
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
// Layout-aware motion
// ---------------------------------------------------------------------------

/// Build the visual row layout that exactly matches what `render` draws.
///
/// Returns a list of rows in top-to-bottom visual order.  Each row is a `Vec`
/// of FLAT indices (flat = project-ordered concatenation, same ordering as
/// `group_by_project` flattening): the indices of the cells that appear in
/// that visual row, left-to-right.
///
/// Per-project wrapping: for each non-empty group (in `projects` order) its
/// worktrees wrap at `cols`.  A group with `k` worktrees produces
/// `ceil(k / cols)` rows; the last row may be partial.  Empty groups
/// contribute NO rows (only a non-selectable header line in `render`).
///
/// `cols` is clamped to ≥ 1.  The flat index increments exactly as `render`
/// does: only inside the per-group cell loop, so empty groups add 0 to `flat`.
pub fn visual_rows(
    projects: &[String],
    worktrees: &[WorktreeView],
    cols: usize,
) -> Vec<Vec<usize>> {
    let cols = cols.max(1);
    let groups = group_by_project(projects, worktrees);
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut flat: usize = 0;

    for group in &groups {
        if group.is_empty() {
            // Empty groups: `render` increments `flat` 0 times. No selectable rows.
            continue;
        }
        // Partition this group's worktrees into rows of `cols`.
        let mut local = 0usize;
        while local < group.len() {
            let end = (local + cols).min(group.len());
            let row: Vec<usize> = (local..end).map(|i| flat + i).collect();
            rows.push(row);
            local += cols;
        }
        flat += group.len();
    }

    rows
}

/// Compute the next selection index after applying `motion` to the given
/// visual row layout produced by [`visual_rows`].
///
/// - Left  : c > 0 → same row, c-1; else r > 0 → last cell of rows[r-1]; else stay.
/// - Right : c < row.len()-1 → same row, c+1; else r < last → rows[r+1][0]; else stay.
/// - Up    : r > 0 → rows[r-1][min(c, rows[r-1].len()-1)]; else stay.
/// - Down  : r < last → rows[r+1][min(c, rows[r+1].len()-1)]; else stay.
/// - First : rows[0][0].
/// - Last{None}    : last cell of last row.
/// - Last{Some(n)} : 1-based flat index n, clamped to [1, total], mapped to actual flat index.
///
/// Returns the new flat `selected`, always in the valid range.
pub fn move_in_layout(rows: &[Vec<usize>], selected: usize, motion: Motion) -> usize {
    if rows.is_empty() {
        return 0;
    }

    // Total number of selectable items = sum of all row lengths.
    let total: usize = rows.iter().map(|r| r.len()).sum();

    // Locate the current selection.
    let mut cur_row = 0usize;
    let mut cur_col = 0usize;
    let mut found = false;
    'outer: for (r, row) in rows.iter().enumerate() {
        for (c, &flat) in row.iter().enumerate() {
            if flat == selected {
                cur_row = r;
                cur_col = c;
                found = true;
                break 'outer;
            }
        }
    }
    if !found {
        // selected not in layout (e.g. stale index after resize) — snap to first.
        return rows[0][0];
    }

    let last_row = rows.len() - 1;

    match motion {
        Motion::Left => {
            if cur_col > 0 {
                rows[cur_row][cur_col - 1]
            } else if cur_row > 0 {
                // Wrap to last cell of the previous row.
                *rows[cur_row - 1].last().unwrap()
            } else {
                selected
            }
        }
        Motion::Right => {
            if cur_col + 1 < rows[cur_row].len() {
                rows[cur_row][cur_col + 1]
            } else if cur_row < last_row {
                // Wrap to first cell of the next row.
                rows[cur_row + 1][0]
            } else {
                selected
            }
        }
        Motion::Up => {
            if cur_row > 0 {
                let c = cur_col.min(rows[cur_row - 1].len() - 1);
                rows[cur_row - 1][c]
            } else {
                selected
            }
        }
        Motion::Down => {
            if cur_row < last_row {
                let c = cur_col.min(rows[cur_row + 1].len() - 1);
                rows[cur_row + 1][c]
            } else {
                selected
            }
        }
        Motion::First => rows[0][0],
        Motion::Last { count: None } => *rows[last_row].last().unwrap(),
        Motion::Last { count: Some(n) } => {
            // 1-based index n into the flat selectable sequence; clamp to [1, total].
            let one_based = n.max(1).min(total);
            // Walk the rows to find the (one_based - 1)th flat index.
            let mut remaining = one_based - 1;
            for row in rows {
                if remaining < row.len() {
                    return row[remaining];
                }
                remaining -= row.len();
            }
            // Fallback (should not reach here after clamping).
            *rows[last_row].last().unwrap()
        }
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
        WorktreeStatus::Deleting => (Color::Red, Color::Red),
    }
}

/// Color for a PR status tag (per the fixed taxonomy).
fn pr_status_colors(pr: &PrStatus) -> Color {
    match pr {
        PrStatus::Loading => Color::Cyan,
        PrStatus::NoPr => Color::DarkGray,
        PrStatus::Draft => Color::DarkGray,
        PrStatus::Open => Color::Yellow,
        PrStatus::ChecksRunning => Color::Yellow,
        PrStatus::ChecksFailing => Color::Red,
        PrStatus::ChecksPassing => Color::LightGreen,
        PrStatus::Approved => Color::Green,
        PrStatus::Merged => Color::Magenta,
        PrStatus::Closed => Color::Red,
    }
}

/// Short human-readable tag for a PR status (kept short for the ~22-wide cell).
fn pr_status_label(pr: &PrStatus) -> &'static str {
    match pr {
        PrStatus::Loading => "loading\u{2026}",
        PrStatus::NoPr => "no PR",
        PrStatus::Draft => "draft",
        PrStatus::Open => "PR open",
        PrStatus::ChecksRunning => "CI running",
        PrStatus::ChecksFailing => "checks \u{2717}",
        PrStatus::ChecksPassing => "checks \u{2713}",
        PrStatus::Approved => "approved",
        PrStatus::Merged => "merged",
        PrStatus::Closed => "closed",
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
        let now = chrono::Utc::now();
        WorktreeView {
            path: PathBuf::from(format!("/wt/{project}/{name}")),
            project: project.to_string(),
            name: name.to_string(),
            branch: "HEAD".to_string(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: PrStatus::NoPr,
            unresolved_comments: None,
            last_summary: None,
            activity: None,
            turns: 0,
            tokens: 0,
            run_started_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn deleting_status_colors_are_red() {
        assert_eq!(
            status_colors(&WorktreeStatus::Deleting),
            (Color::Red, Color::Red)
        );
    }

    // -----------------------------------------------------------------------
    // Loading variant label + color
    // -----------------------------------------------------------------------

    #[test]
    fn pr_status_loading_label_is_loading_ellipsis() {
        assert_eq!(pr_status_label(&PrStatus::Loading), "loading\u{2026}");
    }

    #[test]
    fn pr_status_loading_color_is_cyan() {
        assert_eq!(pr_status_colors(&PrStatus::Loading), Color::Cyan);
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

    // -----------------------------------------------------------------------
    // visual_rows tests
    // -----------------------------------------------------------------------

    // cols=3, project A has 2 worktrees (flat 0,1), project B has 4 (flat 2,3,4,5).
    // Expected rows:
    //   row 0: [0, 1]          (A's only row — partial)
    //   row 1: [2, 3, 4]       (B's first row — full)
    //   row 2: [5]             (B's second row — partial)
    #[test]
    fn visual_rows_multi_project_non_multiple_of_cols() {
        let projects = names(&["A", "B"]);
        let wts = vec![
            wv("A", "a1"),
            wv("A", "a2"),
            wv("B", "b1"),
            wv("B", "b2"),
            wv("B", "b3"),
            wv("B", "b4"),
        ];
        let rows = visual_rows(&projects, &wts, 3);
        assert_eq!(rows.len(), 3, "expected 3 visual rows");
        assert_eq!(rows[0], vec![0, 1]);
        assert_eq!(rows[1], vec![2, 3, 4]);
        assert_eq!(rows[2], vec![5]);
    }

    #[test]
    fn visual_rows_single_project() {
        let projects = names(&["solo"]);
        let wts = vec![wv("solo", "x"), wv("solo", "y"), wv("solo", "z")];
        // cols=2: [0,1], [2]
        let rows = visual_rows(&projects, &wts, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![0, 1]);
        assert_eq!(rows[1], vec![2]);
    }

    #[test]
    fn visual_rows_empty_middle_project_contributes_no_rows() {
        // "beta" is empty; flat indices skip over it.
        let projects = names(&["alpha", "beta", "gamma"]);
        let wts = vec![wv("alpha", "a1"), wv("gamma", "g1"), wv("gamma", "g2")];
        // flat: alpha=0, gamma=1,2
        // cols=3: row0=[0] (alpha), row1=[1,2] (gamma)
        let rows = visual_rows(&projects, &wts, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![0]);
        assert_eq!(rows[1], vec![1, 2]);
    }

    #[test]
    fn visual_rows_cols_one_edge() {
        // cols=1: every worktree is its own row.
        let projects = names(&["p"]);
        let wts = vec![wv("p", "w1"), wv("p", "w2"), wv("p", "w3")];
        let rows = visual_rows(&projects, &wts, 1);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec![0]);
        assert_eq!(rows[1], vec![1]);
        assert_eq!(rows[2], vec![2]);
    }

    #[test]
    fn visual_rows_cols_zero_clamped_to_one() {
        // cols=0 is clamped to 1; same result as cols=1.
        let projects = names(&["p"]);
        let wts = vec![wv("p", "w1"), wv("p", "w2")];
        let rows = visual_rows(&projects, &wts, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![0]);
        assert_eq!(rows[1], vec![1]);
    }

    // -----------------------------------------------------------------------
    // move_in_layout tests
    // -----------------------------------------------------------------------

    // Layout used by many tests below:
    //   cols=3, A=[0,1], B=[2,3,4,5]
    //   row 0: [0, 1]
    //   row 1: [2, 3, 4]
    //   row 2: [5]
    fn make_rows_a2_b4() -> Vec<Vec<usize>> {
        let projects = names(&["A", "B"]);
        let wts = vec![
            wv("A", "a1"),
            wv("A", "a2"),
            wv("B", "b1"),
            wv("B", "b2"),
            wv("B", "b3"),
            wv("B", "b4"),
        ];
        visual_rows(&projects, &wts, 3)
    }

    // REGRESSION: Right from A's last cell (flat 1) must reach B's first (flat 2),
    // not land on the same-row uniform jump that the old apply_motion used.
    #[test]
    fn right_from_last_of_partial_row_crosses_group_boundary() {
        let rows = make_rows_a2_b4();
        // flat 1 is the last cell of row 0 ([0,1]). Right → row1[0] = 2.
        assert_eq!(move_in_layout(&rows, 1, Motion::Right), 2);
    }

    // REGRESSION: Down from flat 0 (A's row) → B's first row, clamped col.
    // With uniform layout (old code, cols=3) Down from 0 = 0+3 = 3.
    // Layout-aware: rows[0]=[0,1], rows[1]=[2,3,4] → col 0 → flat 2. Correct.
    #[test]
    fn down_from_a_row_lands_on_b_first_row_clamped_col() {
        let rows = make_rows_a2_b4();
        assert_eq!(move_in_layout(&rows, 0, Motion::Down), 2);
        assert_eq!(move_in_layout(&rows, 1, Motion::Down), 3); // col 1 → rows[1][1]=3
    }

    #[test]
    fn up_from_b_first_row_lands_on_a_clamped_col() {
        let rows = make_rows_a2_b4();
        // From flat 4 (row1, col2). Up → rows[0][min(2,1)=1] = 1.
        assert_eq!(move_in_layout(&rows, 4, Motion::Up), 1);
        // From flat 2 (row1, col0). Up → rows[0][0] = 0.
        assert_eq!(move_in_layout(&rows, 2, Motion::Up), 0);
    }

    #[test]
    fn left_within_row_does_not_cross_boundary() {
        let rows = make_rows_a2_b4();
        assert_eq!(move_in_layout(&rows, 3, Motion::Left), 2); // within B's row1
    }

    #[test]
    fn left_at_first_cell_stays() {
        let rows = make_rows_a2_b4();
        assert_eq!(move_in_layout(&rows, 0, Motion::Left), 0);
    }

    #[test]
    fn right_at_last_cell_stays() {
        let rows = make_rows_a2_b4();
        assert_eq!(move_in_layout(&rows, 5, Motion::Right), 5);
    }

    #[test]
    fn down_at_last_row_stays() {
        let rows = make_rows_a2_b4();
        assert_eq!(move_in_layout(&rows, 5, Motion::Down), 5);
    }

    #[test]
    fn up_at_first_row_stays() {
        let rows = make_rows_a2_b4();
        assert_eq!(move_in_layout(&rows, 0, Motion::Up), 0);
        assert_eq!(move_in_layout(&rows, 1, Motion::Up), 1);
    }

    #[test]
    fn first_jumps_to_flat_zero() {
        let rows = make_rows_a2_b4();
        assert_eq!(move_in_layout(&rows, 5, Motion::First), 0);
    }

    #[test]
    fn last_none_jumps_to_last_cell() {
        let rows = make_rows_a2_b4();
        assert_eq!(move_in_layout(&rows, 0, Motion::Last { count: None }), 5);
    }

    #[test]
    fn last_some_1_based_index() {
        let rows = make_rows_a2_b4(); // 6 total cells
        assert_eq!(move_in_layout(&rows, 0, Motion::Last { count: Some(1) }), 0);
        assert_eq!(move_in_layout(&rows, 0, Motion::Last { count: Some(2) }), 1);
        assert_eq!(move_in_layout(&rows, 0, Motion::Last { count: Some(3) }), 2);
        assert_eq!(move_in_layout(&rows, 0, Motion::Last { count: Some(6) }), 5);
        // Out-of-range clamps to last.
        assert_eq!(
            move_in_layout(&rows, 0, Motion::Last { count: Some(100) }),
            5
        );
        // 0G clamps to 1 → first cell.
        assert_eq!(move_in_layout(&rows, 5, Motion::Last { count: Some(0) }), 0);
    }

    // REGRESSION: round-trip Down then Up with non-uniform group sizes (cols=3,
    // sizes [2,4]).  Old uniform code: Down from flat 0 would give 0+3=3; Up from
    // 3 gives 3-3=0 ✓ (accidental). Layout-aware: Down(0)=2, Up(2)=0 ✓.
    // The distinguishing case is Down from flat 1 (col 1 of row 0):
    //   old code: 1+3=4; Up(4)=4-3=1 ✓ (still round-trips).
    //   But Down from flat 1 should go to rows[1][1]=3, not 4.
    //   Then Up(3)=rows[0][min(1,1)]=1 ✓ — round-trips correctly.
    #[test]
    fn round_trip_down_up_with_non_uniform_groups() {
        let rows = make_rows_a2_b4();
        let after_down = move_in_layout(&rows, 1, Motion::Down);
        assert_eq!(after_down, 3); // layout-aware, NOT 4 (old uniform mistake)
        let after_up = move_in_layout(&rows, after_down, Motion::Up);
        assert_eq!(after_up, 1); // round-trips back
    }

    // Resize: same logical worktree remains reachable after cols change.
    // Layout: A=[0,1], B=[2,3,4,5].
    //   cols=3: rows = [0,1], [2,3,4], [5]  → flat 3 is at row1 col1.
    //   cols=2: rows = [0,1], [2,3], [4,5]  → flat 3 is at row1 col1.
    // Both are in-range so no out-of-range jump.
    #[test]
    fn resize_no_out_of_range_selection() {
        let projects = names(&["A", "B"]);
        let wts = vec![
            wv("A", "a1"),
            wv("A", "a2"),
            wv("B", "b1"),
            wv("B", "b2"),
            wv("B", "b3"),
            wv("B", "b4"),
        ];
        let rows3 = visual_rows(&projects, &wts, 3);
        let rows2 = visual_rows(&projects, &wts, 2);
        // flat 3 should be locatable in both layouts.
        let found3 = rows3.iter().any(|row| row.contains(&3));
        let found2 = rows2.iter().any(|row| row.contains(&3));
        assert!(found3, "flat 3 must appear in cols=3 layout");
        assert!(found2, "flat 3 must appear in cols=2 layout");
        // Down from flat 1 in cols=3 should NOT produce out-of-range index.
        let next = move_in_layout(&rows3, 1, Motion::Down);
        assert!(next < wts.len(), "result must be a valid flat index");
    }
}
