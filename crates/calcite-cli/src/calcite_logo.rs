//! The calc(ite) banner — shown during parse & compile.
//!
//! The ASCII art has 5 rows; each row is coloured with a warm-to-cool
//! gradient, brightest at top, dimmest at bottom.

const LOGO: &[&str; 5] = &[
    "░░      ░░░░      ░░░  ░░░░░░░░░      ░░░        ░░        ░░        ░",
    "▒  ▒▒▒▒  ▒▒  ▒▒▒▒  ▒▒  ▒▒▒▒▒▒▒▒  ▒▒▒▒  ▒▒▒▒▒  ▒▒▒▒▒▒▒▒  ▒▒▒▒▒  ▒▒▒▒▒▒▒",
    "▓  ▓▓▓▓▓▓▓▓  ▓▓▓▓  ▓▓  ▓▓▓▓▓▓▓▓  ▓▓▓▓▓▓▓▓▓▓▓  ▓▓▓▓▓▓▓▓  ▓▓▓▓▓      ▓▓▓",
    "█  ████  ██        ██  ████████  ████  █████  ████████  █████  ███████",
    "██      ███  ████  ██        ███      ███        █████  █████        █",
];

// Warm descending: bright yellow, orange, red, magenta, purple.
const ROW_COLORS: [(u8, u8, u8); 5] = [
    (255, 225, 120), // pale gold
    (255, 170,  60), // orange
    (230,  80,  80), // red
    (190,  60, 170), // magenta
    (120,  60, 170), // purple
];

pub fn print() {
    eprintln!();
    for (i, line) in LOGO.iter().enumerate() {
        let (r, g, b) = ROW_COLORS[i];
        eprintln!("  \x1b[38;2;{r};{g};{b}m{line}\x1b[0m");
    }
    eprintln!();
}
