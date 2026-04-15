//! The CSS-DOS 32x32 splash — shown during program selection & generation.

const LOGO: &[u8; 32 * 32 * 4] = include_bytes!("../assets/css-dos-logo.rgba");

/// Background shade used wherever the logo is transparent, so the logo's
/// black outline doesn't blend into the terminal's own black background.
const LOGO_BG: (u8, u8, u8) = (80, 80, 80);

fn pixel(x: usize, y: usize) -> (u8, u8, u8) {
    let i = (y * 32 + x) * 4;
    let r = LOGO[i];
    let g = LOGO[i + 1];
    let b = LOGO[i + 2];
    let a = LOGO[i + 3];
    if a < 128 { LOGO_BG } else { (r, g, b) }
}

/// Append the rendered logo to `out`, using ANSI 24-bit truecolor and ▀
/// half-block characters (2 rows of pixels per terminal row → 32x16 cells).
pub fn append(out: &mut String) {
    for row in 0..16 {
        for col in 0..32 {
            let (tr, tg, tb) = pixel(col, row * 2);
            let (br, bg_, bb) = pixel(col, row * 2 + 1);
            out.push_str(&format!(
                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▀",
                tr, tg, tb, br, bg_, bb
            ));
        }
        out.push_str("\x1b[0m\n");
    }
}

pub fn print() {
    let mut s = String::new();
    append(&mut s);
    eprint!("{s}");
}
