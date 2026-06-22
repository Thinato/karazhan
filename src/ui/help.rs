//! Help overlay rendered over the active view when `show_help` is true.
//!
//! Shows all keybindings grouped by view/mode.  Rendered as a centered popup
//! with a bordered block; the underlying view is still visible around it.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

/// Render the help overlay centered in `area`.
pub fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(60, 80, area);

    // Clear the background so the popup is readable.
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" karazhan — keybindings (? / Esc / q to close) ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = help_lines();
    let para = Paragraph::new(lines);
    frame.render_widget(para, inner);
}

fn help_lines() -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let key = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let desc = Style::default().fg(Color::White);
    let dim = Style::default().fg(Color::DarkGray);

    vec![
        // ── Global ──────────────────────────────────────────────────────────
        Line::from(Span::styled("Global", header)),
        Line::from(""),
        kv(
            "Tab     ",
            "switch between Library and Grid view",
            key,
            desc,
        ),
        kv("?       ", "toggle this help overlay", key, desc),
        kv("C-p     ", "open command palette", key, desc),
        kv("q       ", "quit karazhan", key, desc),
        kv("Ctrl-C  ", "quit (always works)", key, desc),
        Line::from(""),
        // ── Library view ────────────────────────────────────────────────────
        Line::from(Span::styled("Library view", header)),
        Line::from(""),
        kv("j / ↓   ", "move selection down", key, desc),
        kv("k / ↑   ", "move selection up", key, desc),
        kv("/       ", "enter filter mode (type to search)", key, desc),
        kv("n / a   ", "create a new prompt", key, desc),
        kv("e       ", "edit selected prompt in $EDITOR", key, desc),
        Line::from(""),
        Line::from(Span::styled(
            "  (filter mode)  Esc: clear filter  Backspace: delete char",
            dim,
        )),
        Line::from(Span::styled(
            "  (new prompt)   Enter: confirm  Esc: cancel",
            dim,
        )),
        Line::from(""),
        // ── Grid view ───────────────────────────────────────────────────────
        Line::from(Span::styled("Grid view", header)),
        Line::from(""),
        kv("h / ←   ", "move selection left", key, desc),
        kv("j / ↓   ", "move selection down", key, desc),
        kv("k / ↑   ", "move selection up", key, desc),
        kv("l / →   ", "move selection right", key, desc),
        kv("g       ", "jump to first worktree", key, desc),
        kv("G       ", "jump to last worktree", key, desc),
        kv(
            "<n>G    ",
            "jump to worktree at index n (e.g. 3G)",
            key,
            desc,
        ),
        kv(
            "c       ",
            "run a custom free-text prompt on selection",
            key,
            desc,
        ),
        kv("p       ", "address all open PR review comments", key, desc),
        kv(
            "i       ",
            "check CI for failures and address them",
            key,
            desc,
        ),
        kv("a       ", "toggle auto-continue on PR merge", key, desc),
        kv("n       ", "new worktree (blank or from prompt)", key, desc),
        kv("N       ", "rename worktree", key, desc),
        kv("r       ", "refresh worktree list", key, desc),
        kv(
            "Q       ",
            "stop the supervisor daemon, then quit",
            key,
            desc,
        ),
        Line::from(""),
        Line::from(Span::styled(
            "  q / Ctrl-C quit the TUI; the daemon (agents + watcher) keeps running.",
            dim,
        )),
        Line::from(Span::styled(
            "  (prompt input)  Enter: send  Esc: cancel  Backspace: delete",
            dim,
        )),
    ]
}

fn kv(k: &'static str, v: &'static str, key_style: Style, val_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(k, key_style),
        Span::styled(v, val_style),
    ])
}

/// Compute a centered [`Rect`] that is `percent_x`% wide and `percent_y`% tall
/// within `area`.  Clamps to at least 1×1.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let w = (area.width * percent_x / 100).max(1).min(area.width);
    let h = (area.height * percent_y / 100).max(1).min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}
