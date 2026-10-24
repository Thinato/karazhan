pub mod grid;
pub mod help;
pub mod keymap;
pub mod library;
pub mod palette;
pub mod terminal;

/// Braille spinner glyph for an animation frame counter (advanced once per UI
/// tick).  Used to signal that a Running agent is alive.
pub fn spinner_glyph(frame: usize) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[frame % FRAMES.len()]
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
}
