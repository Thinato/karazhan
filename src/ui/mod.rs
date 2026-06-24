pub mod detail;
pub mod grid;
pub mod help;
pub mod keymap;
pub mod library;
pub mod palette;

/// Braille spinner glyph for an animation frame counter (advanced once per UI
/// tick).  Used to signal that a Running agent is alive.
pub fn spinner_glyph(frame: usize) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[frame % FRAMES.len()]
}

/// Format a duration in whole seconds compactly: `"45s"`, `"1m02s"`, `"1h03m"`.
pub fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Format a token count: `<1000` verbatim (`"950"`), else one-decimal thousands
/// (`"1.2k"`).
pub fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_glyph_cycles() {
        assert_eq!(spinner_glyph(0), '⠋');
        assert_eq!(spinner_glyph(10), '⠋'); // wraps
        assert_ne!(spinner_glyph(0), spinner_glyph(1));
    }

    #[test]
    fn fmt_elapsed_buckets() {
        assert_eq!(fmt_elapsed(45), "45s");
        assert_eq!(fmt_elapsed(62), "1m02s");
        assert_eq!(fmt_elapsed(600), "10m00s");
        assert_eq!(fmt_elapsed(3780), "1h03m");
    }

    #[test]
    fn fmt_tokens_buckets() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(1200), "1.2k");
        assert_eq!(fmt_tokens(0), "0");
    }
}
