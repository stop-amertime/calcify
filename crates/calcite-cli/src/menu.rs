//! Interactive cabinet picker.
//!
//! When `calcite` is invoked without `-i`, this shows a grid of `.css`
//! cabinets available to run. Arrow keys to navigate, Enter to select,
//! Q / Esc / Ctrl+C to quit.
//!
//! The picker scans `calcite_root` and `calcite_root/cabinets/` for
//! `.css` files. Cabinets are produced upstream (e.g. CSS-DOS's
//! `builder/build.mjs`); calcite-cli does not build them.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use crate::cssdos_logo;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
}

impl Entry {
    pub fn label(&self) -> String {
        format!("{}.css ({})", self.name, human_size(self.bytes))
    }
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// Discover all `.css` cabinets under `calcite_root` and
/// `calcite_root/cabinets/`. Sorted by label.
pub fn discover(calcite_root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    collect_css(calcite_root, &mut out);
    let cabinets = calcite_root.join("cabinets");
    if cabinets.is_dir() {
        collect_css(&cabinets, &mut out);
    }
    out.sort_by(|a, b| a.label().cmp(&b.label()));
    out
}

fn collect_css(dir: &Path, out: &mut Vec<Entry>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|s| s.to_str()) != Some("css") {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()) else { continue };
        let bytes = p.metadata().ok().map(|m| m.len()).unwrap_or(0);
        out.push(Entry { name: name.to_string(), path: p.clone(), bytes });
    }
}

/// Show the menu, return the selected entry index or None if cancelled.
pub fn run(entries: &[Entry]) -> io::Result<Option<usize>> {
    if entries.is_empty() {
        eprintln!("\nNo .css cabinets found. Drop one into the calcite root or cabinets/.");
        return Ok(None);
    }

    terminal::enable_raw_mode()?;
    let result = run_inner(entries);
    let _ = terminal::disable_raw_mode();
    print!("\x1b[?25h"); // show cursor
    let _ = io::stdout().flush();
    result
}

fn run_inner(entries: &[Entry]) -> io::Result<Option<usize>> {
    let mut cursor: usize = 0;
    loop {
        let (cols, _rows) = terminal::size().unwrap_or((80, 24));
        let cell_w: u16 = 44; // 42 chars + 2 padding
        let grid_cols = ((cols / cell_w).max(1)) as usize;

        draw(entries, cursor, grid_cols, cell_w as usize)?;

        let ev = event::read()?;
        if let Event::Key(k) = ev {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match handle_key(k, cursor, entries.len(), grid_cols) {
                KeyAction::Move(n) => cursor = n,
                KeyAction::Select => return Ok(Some(cursor)),
                KeyAction::Quit => return Ok(None),
                KeyAction::None => {}
            }
        }
    }
}

enum KeyAction {
    None,
    Move(usize),
    Select,
    Quit,
}

fn handle_key(k: KeyEvent, cursor: usize, total: usize, cols: usize) -> KeyAction {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Char('c') if ctrl => KeyAction::Quit,
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => KeyAction::Quit,
        KeyCode::Enter => KeyAction::Select,
        KeyCode::Left => KeyAction::Move(if cursor > 0 { cursor - 1 } else { cursor }),
        KeyCode::Right => KeyAction::Move((cursor + 1).min(total - 1)),
        KeyCode::Up => KeyAction::Move(cursor.saturating_sub(cols)),
        KeyCode::Down => {
            let n = cursor + cols;
            KeyAction::Move(if n < total { n } else { cursor })
        }
        KeyCode::Home => KeyAction::Move(0),
        KeyCode::End => KeyAction::Move(total - 1),
        _ => KeyAction::None,
    }
}

fn draw(entries: &[Entry], cursor: usize, cols: usize, cell_w: usize) -> io::Result<()> {
    let mut out = String::new();
    // Full clear + home, then logo + menu every redraw. This way the logo
    // always anchors the top of the screen, no matter how tall the menu is.
    out.push_str("\x1b[2J\x1b[H");
    cssdos_logo::append(&mut out);
    out.push('\n');
    out.push_str("  Pick a cabinet — arrows move, enter runs, q/esc/ctrl-c exits.\n\n");

    for (i, e) in entries.iter().enumerate() {
        let label = ellipsize(&e.label(), cell_w.saturating_sub(2));
        let cell = format!("{:<width$}", label, width = cell_w.saturating_sub(2));
        if i == cursor {
            out.push_str(&format!("\x1b[7m {cell} \x1b[0m"));
        } else {
            out.push_str(&format!(" {cell} "));
        }
        if (i + 1) % cols == 0 {
            out.push('\n');
        }
    }
    if entries.len() % cols != 0 {
        out.push('\n');
    }

    let stdout = io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(out.as_bytes())?;
    lock.flush()?;
    Ok(())
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max <= 1 {
        "…".to_string()
    } else {
        let take = max - 1;
        let mut out: String = s.chars().take(take).collect();
        out.push('…');
        out
    }
}

/// Resolve an entry to a `.css` path. Trivial now that the menu only
/// surfaces pre-built cabinets, but kept as a stable API for `main.rs`.
pub fn resolve_to_css(entry: &Entry) -> PathBuf {
    entry.path.clone()
}
