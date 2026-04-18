//! Interactive program picker.
//!
//! When `calcite` is invoked without `-i`, this shows a grid of available
//! programs (pre-built `.css` in `output/`, plus every top-level `.com`/`.exe`
//! and every cart subdirectory under `programs/`). Arrow keys to navigate,
//! Enter to select, Q / Esc / Ctrl+C to quit.
//!
//! Selecting a pre-built `.css` returns its path directly.
//! Selecting a program invokes `node ../CSS-DOS/builder/build.mjs` with the
//! cart path (either the loose .com/.exe or the subdir), streaming its output
//! below the CSS-DOS logo, then returns the generated `.css` path.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use crate::cssdos_logo;

#[derive(Debug, Clone)]
pub enum Entry {
    /// Pre-built CSS in `output/`.
    PrebuiltCss { name: String, path: PathBuf, bytes: u64 },
    /// A cart to build: either a loose .com/.exe or a subdirectory.
    Program {
        name: String,
        /// Path handed to build.mjs — a loose file or a directory.
        cart: PathBuf,
        bytes: u64,
        /// True if `cart` points at a directory cart.
        is_dir: bool,
    },
}

impl Entry {
    pub fn label(&self) -> String {
        match self {
            Entry::PrebuiltCss { name, bytes, .. } => {
                format!("{name}.css ({})", human_size(*bytes))
            }
            Entry::Program { name, cart, bytes, is_dir } => {
                if *is_dir {
                    format!("{name}/ ({})", human_size(*bytes))
                } else {
                    let ext = cart
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    format!("{name}.{ext} ({})", human_size(*bytes))
                }
            }
        }
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

/// Discover all menu entries. `calcite_root` is the directory containing
/// `output/`, `programs/`, and (as sibling) `../CSS-DOS/`.
pub fn discover(calcite_root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();

    // Pre-built CSS files.
    let output_dir = calcite_root.join("output");
    if output_dir.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&output_dir) {
            let mut css: Vec<_> = rd
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("css") {
                        let name = p.file_stem()?.to_string_lossy().into_owned();
                        let bytes = p.metadata().ok().map(|m| m.len()).unwrap_or(0);
                        Some(Entry::PrebuiltCss { name, path: p, bytes })
                    } else {
                        None
                    }
                })
                .collect();
            css.sort_by(|a, b| a.label().cmp(&b.label()));
            out.extend(css);
        }
    }

    // Every .com / .exe under programs/ recursively.
    let programs_dir = calcite_root.join("programs");
    let mut progs = Vec::new();
    if programs_dir.is_dir() {
        collect_programs(&programs_dir, &programs_dir, &mut progs);
    }
    progs.sort_by(|a, b| a.label().cmp(&b.label()));
    out.extend(progs);

    out
}

fn collect_programs(root: &Path, dir: &Path, out: &mut Vec<Entry>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let dname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if dname == ".cache" {
                continue;
            }
            // Only surface subdirs that actually contain a runnable.
            if !has_runnable(&p) {
                continue;
            }
            let bytes = dir_size(&p);
            let rel = p.strip_prefix(root).unwrap_or(&p);
            let name = rel.to_string_lossy().replace('\\', "/");
            out.push(Entry::Program { name, cart: p, bytes, is_dir: true });
        } else {
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            if ext == "com" || ext == "exe" {
                let bytes = p.metadata().ok().map(|m| m.len()).unwrap_or(0);
                let rel = p.strip_prefix(root).unwrap_or(&p);
                let name = rel.with_extension("").to_string_lossy().replace('\\', "/");
                out.push(Entry::Program { name, cart: p, bytes, is_dir: false });
            }
        }
    }
}

fn has_runnable(dir: &Path) -> bool {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return false,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if ext == "com" || ext == "exe" {
            return true;
        }
    }
    false
}

fn dir_size(dir: &Path) -> u64 {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return 0,
    };
    let mut total = 0u64;
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            total = total.saturating_add(dir_size(&p));
        } else if let Ok(md) = p.metadata() {
            total = total.saturating_add(md.len());
        }
    }
    total
}

/// Show the menu, return the selected entry or None if cancelled.
pub fn run(entries: &[Entry]) -> io::Result<Option<usize>> {
    if entries.is_empty() {
        eprintln!("\nNo programs found. Drop .com/.exe into programs/ or .css into output/.");
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
    // Home + clear-from-cursor (not full screen: logo stays put above).
    let mut out = String::new();
    // Full clear + home, then logo + menu every redraw. This way the logo
    // always anchors the top of the screen, no matter how tall the menu is.
    out.push_str("\x1b[2J\x1b[H");
    cssdos_logo::append(&mut out);
    out.push('\n');
    out.push_str("  Pick a program — arrows move, enter runs, q/esc/ctrl-c exits.\n\n");

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

/// Resolve an entry to a `.css` path, invoking CSS-DOS's builder if needed.
/// Streams the builder's output to stderr. `calcite_root` is the directory
/// containing `output/` and `programs/`.
pub fn resolve_to_css(entry: &Entry, calcite_root: &Path) -> io::Result<PathBuf> {
    match entry {
        Entry::PrebuiltCss { path, .. } => Ok(path.clone()),
        Entry::Program { name, cart, .. } => {
            let output_dir = calcite_root.join("output");
            let _ = std::fs::create_dir_all(&output_dir);
            // Sanitise name for the output file (replace path separators).
            let safe = name.replace(['/', '\\'], "_");
            let out_css = output_dir.join(format!("{safe}.css"));

            // builder/build.mjs lives in the sibling CSS-DOS repo.
            let builder = calcite_root
                .parent()
                .map(|p| p.join("CSS-DOS").join("builder").join("build.mjs"))
                .unwrap_or_else(|| PathBuf::from("../CSS-DOS/builder/build.mjs"));

            eprintln!(
                "\n  Building {} → {} …",
                cart.display(),
                out_css.display()
            );

            let status = Command::new("node")
                .arg("--max-old-space-size=8192")
                .arg(&builder)
                .arg(cart)
                .arg("-o")
                .arg(&out_css)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()?;
            if !status.success() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("build.mjs exited with {status}"),
                ));
            }

            Ok(out_css)
        }
    }
}
