//! Command-palette modal overlay (Ctrl-P).
//!
//! Renders a centered, bordered popup anchored near the top of the screen with
//! a query line, a scrollable list of matching commands, and a footer hint.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::commands::{spec, NewWorktreeModal, Palette};

/// Render the command palette over `area`.
pub fn render_palette(frame: &mut Frame, area: Rect, palette: &Palette) {
    // ---- Popup geometry ----
    let width = 72u16.min(area.width.saturating_sub(2)).max(1);
    let rows = palette.filtered.len() as u16;
    let height = (rows + 5).clamp(7, 24).min(area.height.max(1));

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + area.height / 6;
    // Keep the popup fully on-screen vertically.
    let y = y.min(area.y + area.height.saturating_sub(height));
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Commands ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let inner_w = inner.width as usize;

    // ---- Build lines: query, list rows, footer ----
    let mut lines: Vec<Line> = Vec::new();

    // Query line.
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::styled(
            palette.query.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("█", Style::default().fg(Color::Cyan)),
    ]));

    // How many list rows fit: inner height minus query line and footer line.
    let list_capacity = (inner.height as usize).saturating_sub(2);

    // Scroll so the cursor stays visible.
    let total = palette.filtered.len();
    let offset = if list_capacity == 0 || palette.cursor < list_capacity {
        0
    } else {
        palette.cursor + 1 - list_capacity
    };

    let end = (offset + list_capacity).min(total);
    for row in offset..end {
        let visible_idx = palette.filtered[row];
        let id = palette.visible[visible_idx];
        let s = spec(id);
        let selected = row == palette.cursor;

        let prefix = if selected { "▸ " } else { "  " };
        let raw = format!(
            "{prefix}{title}    {desc}    {key}",
            prefix = prefix,
            title = s.title,
            desc = s.description,
            key = s.keybind,
        );
        let text = truncate(&raw, inner_w);

        let style = if selected {
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    // Pad so the footer sits on the last row.
    while lines.len() + 1 < inner.height as usize {
        lines.push(Line::from(""));
    }

    // Footer hint.
    let footer = truncate(
        "type to filter · ↑↓/C-n C-p move · Enter run · Esc cancel",
        inner_w,
    );
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the new-worktree modal over `area`.  Mirrors [`render_palette`]:
/// centered rounded block, query line, scrollable list, footer hint.
pub fn render_new_worktree(frame: &mut Frame, area: Rect, modal: &NewWorktreeModal) {
    // ---- Popup geometry ----
    let width = 72u16.min(area.width.saturating_sub(2)).max(1);
    let rows = modal.filtered.len() as u16;
    let height = (rows + 5).clamp(7, 24).min(area.height.max(1));

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + area.height / 6;
    let y = y.min(area.y + area.height.saturating_sub(height));
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" New worktree ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let inner_w = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Query line.
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::styled(
            modal.query.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("█", Style::default().fg(Color::Cyan)),
    ]));

    let list_capacity = (inner.height as usize).saturating_sub(2);
    let total = modal.filtered.len();
    let offset = if list_capacity == 0 || modal.cursor < list_capacity {
        0
    } else {
        modal.cursor + 1 - list_capacity
    };

    let end = (offset + list_capacity).min(total);
    for row in offset..end {
        let opt_idx = modal.filtered[row];
        let choice = &modal.options[opt_idx];
        let selected = row == modal.cursor;

        let prefix = if selected { "▸ " } else { "  " };
        let raw = format!(
            "{prefix}{label}",
            prefix = prefix,
            label = choice_label(choice)
        );
        let text = truncate(&raw, inner_w);

        let style = if selected {
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    while lines.len() + 1 < inner.height as usize {
        lines.push(Line::from(""));
    }

    let footer = truncate("type to filter · Enter create · Esc cancel", inner_w);
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Label for a worktree choice row.
fn choice_label(choice: &crate::commands::WorktreeChoice) -> String {
    use crate::commands::WorktreeChoice;
    match choice {
        WorktreeChoice::Blank => "blank worktree".to_string(),
        WorktreeChoice::Prompt { title, .. } => title.clone(),
    }
}

/// Truncate a string to at most `max` columns, appending `…` if truncated.
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
