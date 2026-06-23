use chrono::{DateTime, Utc};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ipc::WorktreeView;
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
    pub fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        worktree: Option<&WorktreeView>,
        summary: Option<&str>,
        prompt_input: Option<&str>,
        status_line: Option<&str>,
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

        let lines = build_detail_lines(wt, summary, prompt_input);
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
        _ => "idle",
    };
    lines.push(kv_line(
        "agent       ",
        agent_label,
        key_style,
        status_style,
    ));

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
    }
}

/// Human-readable label for a PR status (full words for the detail card).
fn pr_status_label(pr: &PrStatus) -> &'static str {
    match pr {
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
/// The detail pane is a fixed 36 columns wide; the grid takes the remainder.
pub fn split_grid_detail(area: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(36)])
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
