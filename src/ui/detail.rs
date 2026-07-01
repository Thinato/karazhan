use chrono::{DateTime, Utc};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ipc::WorktreeView;
use crate::ui::{fmt_elapsed, fmt_tokens, spinner_glyph};
use crate::worktree::{PrStatus, WorktreeStatus};

// ---------------------------------------------------------------------------
// DetailView
// ---------------------------------------------------------------------------

/// Renders detailed information about the currently selected worktree.
///
/// Displayed fields:
/// - path
/// - branch
/// - prompt_slug (if set)
/// - pr_number (if set)
/// - auto_continue_on_merge flag
/// - status
/// - live agent status / last-line summary (no raw transcript)
pub struct DetailView;

impl DetailView {
    pub fn new() -> Self {
        Self
    }

    /// Render the detail pane for `worktree` into `area`.
    ///
    /// `summary` is the latest short agent summary (if any), `prompt_input`
    /// is the in-progress free-text prompt while the grid is in prompt-input
    /// mode, and `status_line` is an optional message shown at the bottom of
    /// the pane (backend name, gh errors, transient notifications, …).
    /// If `worktree` is `None` (empty grid), shows a placeholder message.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        worktree: Option<&WorktreeView>,
        summary: Option<&str>,
        prompt_input: Option<&str>,
        status_line: Option<&str>,
        spinner_frame: usize,
    ) {
        let block = Block::default()
            .title(" worktree detail ")
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Split inner into content area + 1-row status line at the bottom.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        let content_area = chunks[0];
        let status_area = chunks[1];

        let Some(wt) = worktree else {
            let msg = Paragraph::new("no worktree selected")
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center);
            frame.render_widget(msg, content_area);
            render_status_line(frame, status_area, status_line);
            return;
        };

        let lines = build_detail_lines(wt, summary, prompt_input, spinner_frame);
        let para = Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        frame.render_widget(para, content_area);

        render_status_line(frame, status_area, status_line);
    }
}

impl Default for DetailView {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Detail line builder
// ---------------------------------------------------------------------------

fn build_detail_lines(
    wt: &WorktreeView,
    summary: Option<&str>,
    prompt_input: Option<&str>,
    spinner_frame: usize,
) -> Vec<Line<'static>> {
    let key_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let val_style = Style::default().fg(Color::White);
    let status_style = status_val_style(&wt.status);

    let path_str = wt.path.display().to_string();

    let mut lines = vec![
        kv_line("project     ", &wt.project, key_style, val_style),
        kv_line("name        ", &wt.name, key_style, val_style),
        kv_line("path        ", &path_str, key_style, val_style),
        kv_line("branch      ", &wt.branch, key_style, val_style),
    ];

    if let Some(slug) = &wt.prompt_slug {
        lines.push(kv_line("prompt      ", slug, key_style, val_style));
    } else {
        lines.push(kv_line("prompt      ", "(none)", key_style, dim_style()));
    }

    // PR line: colored PR status + number when known (e.g. "PR #123 — merged").
    let pr_value = match wt.pr_number {
        Some(pr) => format!("#{pr} — {}", pr_status_label(&wt.pr_status)),
        None => pr_status_label(&wt.pr_status).to_string(),
    };
    lines.push(kv_line(
        "PR          ",
        &pr_value,
        key_style,
        pr_status_val_style(&wt.pr_status),
    ));

    // Unresolved review-comment count.  Bright when > 0 to draw attention; dim
    // "0" when there are none; "—" when unknown / no open PR.
    let (unresolved_value, unresolved_style) = match wt.unresolved_comments {
        Some(n) if n > 0 => (
            n.to_string(),
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ),
        Some(_) => ("0".to_string(), val_style),
        None => ("—".to_string(), dim_style()),
    };
    lines.push(kv_line(
        "unresolved  ",
        &unresolved_value,
        key_style,
        unresolved_style,
    ));

    let auto_str = if wt.auto_continue_on_merge {
        "yes"
    } else {
        "no"
    };
    lines.push(kv_line("auto-cont   ", auto_str, key_style, val_style));

    lines.push(kv_line(
        "status      ",
        status_label(&wt.status),
        key_style,
        status_style,
    ));

    // Timestamps.
    let created_str = wt.created_at.format("%Y-%m-%d %H:%M").to_string();
    lines.push(kv_line(
        "created     ",
        &created_str,
        key_style,
        dim_style(),
    ));

    let now = Utc::now();
    let last_used_rel = humanize_since(wt.updated_at, now);
    // Append a dim "(stale)" marker when the worktree has not been used for 7+ days.
    let last_used_str = if (now - wt.updated_at).num_days() >= 7 {
        format!("{last_used_rel} (stale)")
    } else {
        last_used_rel
    };
    lines.push(kv_line(
        "last used   ",
        &last_used_str,
        key_style,
        dim_style(),
    ));

    // Blank separator.
    lines.push(Line::from(""));

    // Live agent status mirrors the worktree status (coarse, no transcript).
    let agent_label = match wt.status {
        WorktreeStatus::Running => "running",
        WorktreeStatus::NeedsReview => "done (needs review)",
        WorktreeStatus::Error => "error",
        WorktreeStatus::Deleting => "deleting",
        _ => "idle",
    };
    lines.push(kv_line(
        "agent       ",
        agent_label,
        key_style,
        status_style,
    ));

    // Live progress block.  While Running, show the current action (with an
    // animated spinner) and a wall-clock elapsed timer.  Turn/token counters are
    // shown whenever they are non-zero (so they linger after a run finishes).
    if matches!(wt.status, WorktreeStatus::Running) {
        let activity = wt.activity.as_deref().unwrap_or("working…");
        lines.push(kv_line(
            "activity    ",
            &format!("{} {activity}", spinner_glyph(spinner_frame)),
            key_style,
            status_style,
        ));
        if let Some(start) = wt.run_started_at {
            let secs = (Utc::now() - start).num_seconds().max(0) as u64;
            lines.push(kv_line(
                "elapsed     ",
                &fmt_elapsed(secs),
                key_style,
                val_style,
            ));
        }
    }
    if wt.turns > 0 || wt.tokens > 0 {
        lines.push(kv_line(
            "turns       ",
            &wt.turns.to_string(),
            key_style,
            val_style,
        ));
        lines.push(kv_line(
            "tokens      ",
            &fmt_tokens(wt.tokens),
            key_style,
            val_style,
        ));
    }

    // Last-line summary surfaced from the agent (truncated upstream).
    if let Some(s) = summary {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("summary", key_style)));
        lines.push(Line::from(Span::styled(s.to_string(), val_style)));
    }

    // In-progress prompt input (grid prompt-input mode).
    if let Some(input) = prompt_input {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "run prompt (Enter=send, Esc=cancel):",
            Style::default().fg(Color::Cyan),
        )));
        lines.push(Line::from(Span::styled(
            format!("> {input}"),
            Style::default().fg(Color::White),
        )));
    }

    lines
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn kv_line(key: &str, value: &str, key_style: Style, val_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(key.to_string(), key_style),
        Span::styled(value.to_string(), val_style),
    ])
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn status_label(status: &WorktreeStatus) -> &'static str {
    match status {
        WorktreeStatus::Idle => "Idle",
        WorktreeStatus::Running => "Running",
        WorktreeStatus::NeedsReview => "Needs Review",
        WorktreeStatus::CIFailing => "CI Failing",
        WorktreeStatus::PRMerged => "PR Merged",
        WorktreeStatus::Error => "Error",
        WorktreeStatus::Deleting => "Deleting…",
    }
}

/// Human-readable label for a PR status (full words for the detail card).
fn pr_status_label(pr: &PrStatus) -> &'static str {
    match pr {
        PrStatus::Loading => "loading\u{2026}",
        PrStatus::NoPr => "no PR",
        PrStatus::Draft => "draft",
        PrStatus::Open => "open",
        PrStatus::ChecksRunning => "CI running",
        PrStatus::ChecksFailing => "checks failing",
        PrStatus::ChecksPassing => "checks passing",
        PrStatus::Approved => "approved",
        PrStatus::Merged => "merged",
        PrStatus::Closed => "closed",
    }
}

/// Color style for a PR status value (per the fixed taxonomy).
fn pr_status_val_style(pr: &PrStatus) -> Style {
    let color = match pr {
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
    };
    Style::default().fg(color)
}

fn status_val_style(status: &WorktreeStatus) -> Style {
    let color = match status {
        WorktreeStatus::Idle => Color::Gray,
        WorktreeStatus::Running => Color::Yellow,
        WorktreeStatus::NeedsReview => Color::Magenta,
        WorktreeStatus::CIFailing => Color::Red,
        WorktreeStatus::PRMerged => Color::Green,
        WorktreeStatus::Error => Color::Red,
        WorktreeStatus::Deleting => Color::Red,
    };
    let base = Style::default().fg(color);
    match status {
        WorktreeStatus::Error => base.add_modifier(Modifier::BOLD),
        _ => base,
    }
}

// ---------------------------------------------------------------------------
// Status line helper
// ---------------------------------------------------------------------------

/// Render a single-row status line at the bottom of the detail pane.
///
/// Shows the backend name + transient messages (gh errors, PR merged, CI
/// status, etc.).  Falls back to a default hint when no message is set.
fn render_status_line(frame: &mut Frame, area: Rect, status_line: Option<&str>) {
    frame.render_widget(Clear, area);
    let text = status_line.unwrap_or("Tab: switch view  ?: help  q: quit");
    let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(para, area);
}

// ---------------------------------------------------------------------------
// Timestamp helpers
// ---------------------------------------------------------------------------

/// Format a duration as a human-readable "time since" string.
///
/// Takes `now` as a parameter so the function is unit-testable without
/// mocking `Utc::now()`.
pub fn humanize_since(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

// ---------------------------------------------------------------------------
// Layout helper used by ui/mod.rs
// ---------------------------------------------------------------------------

/// Split `area` into [grid_area, detail_area] side by side.
///
/// Default detail-pane width in columns.
pub const DEFAULT_DETAIL_WIDTH: u16 = 36;

/// Resolve the effective detail-pane width: the user-requested width clamped to
/// a minimum of 30 columns and a maximum of 80% of the terminal width.  The same
/// value MUST be used for rendering and for grid-motion column math, or the two
/// disagree and the selection drifts on resize.
pub fn effective_detail_width(term_w: u16, requested: u16) -> u16 {
    let hi = ((term_w as u32 * 4) / 5).max(30) as u16;
    requested.clamp(30, hi)
}

/// Split `area` into (grid, detail).  The detail pane is `requested_detail_width`
/// columns wide (clamped by [`effective_detail_width`]); the grid takes the
/// remainder (at least 20 columns).
pub fn split_grid_detail(area: Rect, requested_detail_width: u16) -> (Rect, Rect) {
    let dw = effective_detail_width(area.width, requested_detail_width);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(dw)])
        .split(area);
    (chunks[0], chunks[1])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(rfc3339: &str) -> DateTime<Utc> {
        rfc3339.parse().unwrap()
    }

    #[test]
    fn effective_detail_width_clamps() {
        // Within bounds → unchanged.
        assert_eq!(effective_detail_width(200, 36), 36);
        // Below the 30-col floor → 30.
        assert_eq!(effective_detail_width(200, 10), 30);
        // Above the 80% cap (160) → 160.
        assert_eq!(effective_detail_width(200, 999), 160);
        // Tiny terminal: 80% (24) is below the floor, so the 30 floor wins.
        assert_eq!(effective_detail_width(30, 36), 30);
    }

    #[test]
    fn deleting_status_label_and_style() {
        assert_eq!(status_label(&WorktreeStatus::Deleting), "Deleting…");
        assert_eq!(
            status_val_style(&WorktreeStatus::Deleting),
            Style::default().fg(Color::Red)
        );
    }

    // -----------------------------------------------------------------------
    // Loading variant label + color
    // -----------------------------------------------------------------------

    #[test]
    fn pr_status_loading_label_is_loading_ellipsis() {
        assert_eq!(
            pr_status_label(&crate::worktree::PrStatus::Loading),
            "loading\u{2026}"
        );
    }

    #[test]
    fn pr_status_loading_val_style_is_cyan() {
        let style = pr_status_val_style(&crate::worktree::PrStatus::Loading);
        assert_eq!(style, Style::default().fg(Color::Cyan));
    }

    #[test]
    fn humanize_since_just_now() {
        let then = ts("2024-01-01T12:00:00Z");
        let now = ts("2024-01-01T12:00:30Z"); // 30 seconds later
        assert_eq!(humanize_since(then, now), "just now");
    }

    #[test]
    fn humanize_since_minutes() {
        let then = ts("2024-01-01T12:00:00Z");
        let now = ts("2024-01-01T12:05:00Z"); // 5 minutes later
        assert_eq!(humanize_since(then, now), "5m ago");
    }

    #[test]
    fn humanize_since_hours() {
        let then = ts("2024-01-01T09:00:00Z");
        let now = ts("2024-01-01T12:00:00Z"); // 3 hours later
        assert_eq!(humanize_since(then, now), "3h ago");
    }

    #[test]
    fn humanize_since_days() {
        let then = ts("2024-01-01T12:00:00Z");
        let now = ts("2024-01-03T12:00:00Z"); // 2 days later
        assert_eq!(humanize_since(then, now), "2d ago");
    }

    #[test]
    fn humanize_since_exactly_one_minute() {
        let then = ts("2024-06-01T00:00:00Z");
        let now = ts("2024-06-01T00:01:00Z"); // exactly 60 seconds
        assert_eq!(humanize_since(then, now), "1m ago");
    }

    #[test]
    fn humanize_since_clock_skew_does_not_panic() {
        // now < then (clock skew / test artifact) — should return "just now"
        let then = ts("2024-01-01T12:00:10Z");
        let now = ts("2024-01-01T12:00:00Z");
        assert_eq!(humanize_since(then, now), "just now");
    }
}
