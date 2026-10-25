//! Grid-view terminal pane: layout math and rendering.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};

use tui_term::widget::PseudoTerminal;

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

/// Pane title listing the live slots, the active one bracketed: `[1] 2 3`.
pub fn pane_title(slots: &[u8], active: Option<u8>) -> String {
    let names: Vec<String> = slots
        .iter()
        .map(|&s| {
            if Some(s) == active {
                format!("[{s}]")
            } else {
                s.to_string()
            }
        })
        .collect();
    names.join(" ")
}

/// Render the attached session's screen (`None` until its first frame).  The
/// border is highlighted while the pane has focus.
pub fn render_term(
    frame: &mut Frame,
    area: Rect,
    screen: Option<&vt100::Screen>,
    title: &str,
    focused: bool,
) {
    let border = if focused {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .title(format!(" {title} "))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border);
    match screen {
        Some(screen) => frame.render_widget(PseudoTerminal::new(screen).block(block), area),
        None => frame.render_widget(block, area),
    }
}

/// Encode a key as the bytes an xterm sends for it.  `app_cursor` is the
/// inner app's application-cursor mode (DECCKM), which switches unmodified
/// arrows and Home/End from `ESC [` to `ESC O`.
pub fn key_to_bytes(key: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let letter = |c: char, ss3: bool| match (m, ss3) {
        (1, true) => format!("\x1bO{c}"),
        (1, false) => format!("\x1b[{c}"),
        _ => format!("\x1b[1;{m}{c}"),
    };
    let tilde = |n: u8| match m {
        1 => format!("\x1b[{n}~"),
        _ => format!("\x1b[{n};{m}~"),
    };
    let seq = match key.code {
        KeyCode::Up => letter('A', app_cursor),
        KeyCode::Down => letter('B', app_cursor),
        KeyCode::Right => letter('C', app_cursor),
        KeyCode::Left => letter('D', app_cursor),
        KeyCode::Home => letter('H', app_cursor),
        KeyCode::End => letter('F', app_cursor),
        KeyCode::F(n @ 1..=4) => letter((b'P' + n - 1) as char, true),
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::BackTab => "\x1b[Z".to_string(),
        _ => {
            // Plain keys: Alt is an ESC prefix rather than a parameter.
            let mut out = match key.code {
                KeyCode::Char(c) if ctrl => match c {
                    'a'..='z' | 'A'..='Z' => vec![c.to_ascii_lowercase() as u8 - b'a' + 1],
                    ' ' | '@' => vec![0],
                    '[' | '\\' | ']' | '^' | '_' => vec![c as u8 - b'@'],
                    // Legacy crossterm reports 0x1c..=0x1f as Ctrl-4..=7.
                    '4'..='7' => vec![c as u8 - b'4' + 0x1c],
                    _ => c.to_string().into_bytes(),
                },
                KeyCode::Char(c) => c.to_string().into_bytes(),
                KeyCode::Enter => vec![b'\r'],
                KeyCode::Tab => vec![b'\t'],
                KeyCode::Backspace => vec![0x7f],
                KeyCode::Esc => vec![0x1b],
                _ => return Vec::new(),
            };
            if alt {
                out.insert(0, 0x1b);
            }
            return out;
        }
    };
    seq.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn key_to_bytes_table() {
        let k = |code, mods| KeyEvent::new(code, mods);
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let cases: &[(KeyEvent, bool, &[u8])] = &[
            (k(KeyCode::Char('c'), ctrl), false, b"\x03"),
            (k(KeyCode::Char('a'), ctrl), false, b"\x01"),
            (k(KeyCode::Char(' '), ctrl), false, b"\x00"),
            (k(KeyCode::Char(']'), ctrl), false, b"\x1d"),
            (k(KeyCode::Char('b'), KeyModifiers::ALT), false, b"\x1bb"),
            (k(KeyCode::Char('é'), none), false, "é".as_bytes()),
            (k(KeyCode::Enter, none), false, b"\r"),
            (k(KeyCode::Tab, none), false, b"\t"),
            (k(KeyCode::BackTab, KeyModifiers::SHIFT), false, b"\x1b[Z"),
            (k(KeyCode::Backspace, none), false, b"\x7f"),
            (k(KeyCode::Esc, none), false, b"\x1b"),
            (k(KeyCode::Up, none), false, b"\x1b[A"),
            (k(KeyCode::Up, none), true, b"\x1bOA"),
            (k(KeyCode::Right, ctrl), false, b"\x1b[1;5C"),
            (k(KeyCode::Right, ctrl), true, b"\x1b[1;5C"),
            (k(KeyCode::Home, none), false, b"\x1b[H"),
            (k(KeyCode::PageUp, none), false, b"\x1b[5~"),
            (k(KeyCode::Delete, none), false, b"\x1b[3~"),
            (k(KeyCode::F(1), none), false, b"\x1bOP"),
            (k(KeyCode::F(5), none), false, b"\x1b[15~"),
            (k(KeyCode::F(12), none), false, b"\x1b[24~"),
        ];
        for (key, app_cursor, want) in cases {
            assert_eq!(key_to_bytes(*key, *app_cursor), *want, "{key:?}");
        }
    }

    #[test]
    fn pane_title_marks_active() {
        assert_eq!(pane_title(&[1, 2, 3], Some(1)), "[1] 2 3");
        assert_eq!(pane_title(&[1, 2, 3], Some(3)), "1 2 [3]");
        assert_eq!(pane_title(&[2, 5], None), "2 5");
    }

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
